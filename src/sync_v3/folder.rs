//! `rr mkdir` on sync v3: a folder is one `.metadata` blob.
//!
//! The shape is copied from folders the vendor's own software wrote, read
//! with `rr inspect` from a real library: one blob, eight fields, keys in
//! alphabetical order, a 4-space indent and one trailing newline. The
//! write goes through the push's own write path in the parent module, so
//! the rewritten root is built under the push's invariant (exactly one
//! added line; every line the server sent is re-emitted byte for byte)
//! and the root pointer moves through the same generation-guarded swap.
//! A test below keeps this file from growing a write loop of its own.

use serde::Serialize;

use crate::error::{Error, Result};

use super::{CloudFile, DocumentInfo, Listing, PushPlan, SyncClient, UploadResult};

/// The parent value the cloud gives an item in the trash.
const TRASH: &str = "trash";

/// A folder ready to be written: its id and its `.metadata` bytes.
#[derive(Debug, Clone)]
pub struct FolderBundle {
    pub doc_uuid: String,
    pub metadata_json: String,
}

/// The eight fields of a folder the vendor's software wrote. Declared in
/// alphabetical order, which is the order they are serialized in.
#[derive(Serialize)]
struct FolderMetadata<'a> {
    #[serde(rename = "createdTime")]
    created_time: &'a str,
    #[serde(rename = "lastModified")]
    last_modified: &'a str,
    new: bool,
    parent: &'a str,
    pinned: bool,
    source: &'a str,
    #[serde(rename = "type")]
    type_field: &'a str,
    #[serde(rename = "visibleName")]
    visible_name: &'a str,
}

impl FolderBundle {
    /// Build the folder `name` with id `doc_uuid`. `parent` is the parent
    /// folder's id, or the empty string for the top level; `now_ms` is the
    /// time in epoch milliseconds, as the string the cloud stores. Both
    /// timestamps take that one reading of the clock.
    pub fn new(doc_uuid: &str, name: &str, parent: &str, now_ms: &str) -> Result<Self> {
        let meta = FolderMetadata {
            created_time: now_ms,
            last_modified: now_ms,
            new: false,
            parent,
            pinned: false,
            source: "",
            type_field: "CollectionType",
            visible_name: name,
        };
        Ok(Self {
            doc_uuid: doc_uuid.to_string(),
            metadata_json: folder_json(&meta)?,
        })
    }

    /// The folder's blobs: `.metadata` alone, the smallest shape the
    /// vendor's own software writes.
    fn files(&self) -> Vec<CloudFile> {
        vec![CloudFile {
            cloud_name: format!("{}.metadata", self.doc_uuid),
            bytes: self.metadata_json.clone().into_bytes(),
        }]
    }
}

/// Serialize as the vendor does: keys in field order, a 4-space indent,
/// and one trailing newline. The index size is this byte length.
fn folder_json(meta: &FolderMetadata<'_>) -> Result<String> {
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    meta.serialize(&mut ser)?;
    buf.push(b'\n');
    String::from_utf8(buf).map_err(|e| Error::Other(format!("folder metadata: {e}")))
}

/// Refuse a folder the tablet could not show where it was asked for, or
/// one that would sit beside an item of the same name. Pure: it works on
/// a listing read beforehand. `parent` is `None` for the top level.
pub fn check_new_folder(listing: &Listing, name: &str, parent: Option<&str>) -> Result<()> {
    if let Some(summary) = listing.incomplete_summary() {
        return refuse(&format!("the listing is incomplete. {summary}"));
    }
    check_name(name)?;
    if let Some(id) = parent {
        check_parent(&listing.docs, id)?;
    }
    for d in &listing.docs {
        if !d.deleted && d.parent.as_deref() == parent && d.visible_name == name {
            return refuse(&format!("{name:?} already exists there ({})", d.id));
        }
    }
    Ok(())
}

/// One name, as the tablet will show it: not empty, no leading or
/// trailing space, no control character, and no `/`, since it is not a
/// path.
fn check_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return refuse("the name is empty");
    }
    if name.trim() != name {
        return refuse("the name starts or ends with a space");
    }
    if name.chars().any(char::is_control) {
        return refuse("the name holds a control character");
    }
    if name.contains('/') {
        return refuse("the name holds `/`; give one name, not a path");
    }
    Ok(())
}

/// The parent must be a live folder, and so must every folder above it,
/// up to the top level. A chain that reaches the trash, or an id the
/// listing does not hold, is not a place the tablet shows.
fn check_parent(docs: &[DocumentInfo], id: &str) -> Result<()> {
    let mut next = id;
    for _ in 0..=docs.len() {
        let Some(folder) = find_doc(docs, next) else {
            return refuse(&format!("no item with id {next} in the listing"));
        };
        if !folder.is_folder() || folder.deleted {
            return refuse(&format!("{next} is not a live folder"));
        }
        match folder.parent.as_deref() {
            None => return Ok(()),
            Some(TRASH) => return refuse(&format!("{next} is in the trash")),
            Some(up) => next = up,
        }
    }
    refuse("the parent chain loops")
}

fn find_doc<'a>(docs: &'a [DocumentInfo], id: &str) -> Option<&'a DocumentInfo> {
    docs.iter().find(|d| d.id == id)
}

fn refuse(why: &str) -> Result<()> {
    Err(Error::Other(format!("mkdir refused: {why}")))
}

impl SyncClient {
    /// Dry run of `upload_folder`: the plan a push dry run makes, for the
    /// folder's one blob. Sends nothing.
    pub async fn plan_folder(&self, folder: &FolderBundle) -> Result<PushPlan> {
        let files = folder.files();
        self.plan_doc(folder.doc_uuid.clone(), files).await
    }

    /// Write `folder` through the push's own write path: the same blob
    /// upload, root plan and generation-guarded root swap, one copy of it.
    pub async fn upload_folder(&self, folder: &FolderBundle) -> Result<UploadResult> {
        let files = folder.files();
        self.upload_doc(folder.doc_uuid.clone(), files).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::sync_v3::{
        diff_root, entry_for, parse_index, plan_root, IndexEntry, RootState, SkippedEntry,
    };

    const FOLDER: &str = "CollectionType";
    const DOC: &str = "DocumentType";
    const NOW: &str = "1789000000000";
    const PARENT_ID: &str = "0e6f2c1a-5b7d-4c3e-9f80-1a2b3c4d5e6f";

    fn item(id: &str, name: &str, parent: Option<&str>, doc_type: &str) -> DocumentInfo {
        DocumentInfo {
            id: id.into(),
            visible_name: name.into(),
            doc_type: doc_type.into(),
            parent: parent.map(str::to_owned),
            deleted: false,
        }
    }

    fn listing(docs: Vec<DocumentInfo>) -> Listing {
        Listing {
            docs,
            skipped: vec![],
        }
    }

    fn library() -> Listing {
        listing(vec![
            item("top", "Work", None, FOLDER),
            item("mid", "Projects", Some("top"), FOLDER),
            item("old", "Old", Some(TRASH), FOLDER),
            item("low", "Kept", Some("old"), FOLDER),
            item("note", "A note", Some("top"), DOC),
        ])
    }

    #[test]
    fn folder_metadata_is_the_vendor_shape() {
        let folder = FolderBundle::new("f1", "Meeting Notes Q3", "", NOW).unwrap();
        let expected = r#"{
    "createdTime": "1789000000000",
    "lastModified": "1789000000000",
    "new": false,
    "parent": "",
    "pinned": false,
    "source": "",
    "type": "CollectionType",
    "visibleName": "Meeting Notes Q3"
}
"#;
        assert_eq!(folder.metadata_json, expected);
    }

    #[test]
    fn folder_metadata_sizes_match_three_vendor_folders() {
        // Byte counts read with `rr inspect` from three folders the
        // vendor's software wrote, two at the top level and one nested under
        // its parent's id. The names here are stand-ins of the same length.
        let first = FolderBundle::new("f1", "Meeting Notes Q3", "", NOW).unwrap();
        let second = FolderBundle::new("f2", "Drafts", "", NOW).unwrap();
        let nested = FolderBundle::new("f3", "Books", PARENT_ID, NOW).unwrap();
        assert_eq!(first.metadata_json.len(), 220);
        assert_eq!(second.metadata_json.len(), 210);
        assert_eq!(nested.metadata_json.len(), 245);
    }

    #[test]
    fn folder_is_one_metadata_blob() {
        let folder = FolderBundle::new("f1", "Meeting Notes Q3", "", NOW).unwrap();
        let files = folder.files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].cloud_name, "f1.metadata");
        assert_eq!(files[0].bytes, folder.metadata_json.as_bytes());
    }

    #[test]
    fn folder_adds_exactly_one_root_line_and_keeps_the_rest() {
        let body = "3\naa:80000000:doc1:4:100\nbb:deadbeef:doc2:2:200\n";
        let (schema, entries) = parse_index(body.as_bytes()).unwrap();
        let root = RootState {
            schema,
            root_hash: "old".into(),
            generation: 7,
            entries,
            body: body.to_string(),
        };
        let folder = FolderBundle::new("f1", "Meeting Notes Q3", "", NOW).unwrap();
        let doc_entries: Vec<IndexEntry> = folder.files().iter().map(entry_for).collect();
        let planned = plan_root(&root, "f1", &doc_entries).unwrap();

        let new_body = String::from_utf8(planned.root_body).unwrap();
        let diff = diff_root(root.schema, &root.body, &new_body);
        assert!(diff.removed.is_empty());
        assert!(diff.order_preserved);
        assert_eq!(diff.new_entries, 3);
        assert_eq!(diff.added.len(), 1);
        assert!(diff.added[0].ends_with(":80000000:f1:1:220"));

        let doc_body = String::from_utf8(planned.doc_body).unwrap();
        let lines: Vec<&str> = doc_body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "3");
        assert!(lines[1].ends_with(":0:f1.metadata:0:220"));
    }

    #[test]
    fn folder_writes_only_through_the_push_write_path() {
        // The code above the tests reaches the cloud only through the two
        // helpers a push uses, so there is one write loop, not two.
        let src = include_str!("folder.rs");
        let (code, _) = src.split_once("#[cfg(test)]").unwrap();
        for needle in ["put_blob(", "update_root(", "plan_root("] {
            assert!(!code.contains(needle), "{needle}");
        }
        assert!(code.contains("self.upload_doc("));
        assert!(code.contains("self.plan_doc("));
    }

    #[test]
    fn new_folder_is_allowed_where_the_name_is_free() {
        let lib = library();
        assert!(check_new_folder(&lib, "Meeting Notes Q3", None).is_ok());
        assert!(check_new_folder(&lib, "Books", Some("mid")).is_ok());
        // The same name elsewhere, or only in the trash, is not a clash.
        assert!(check_new_folder(&lib, "Projects", None).is_ok());
        assert!(check_new_folder(&lib, "Old", None).is_ok());
    }

    #[test]
    fn new_folder_refuses_a_name_already_there() {
        let lib = library();
        assert!(check_new_folder(&lib, "Work", None).is_err());
        assert!(check_new_folder(&lib, "Projects", Some("top")).is_err());
        // A document of that name counts too.
        assert!(check_new_folder(&lib, "A note", Some("top")).is_err());
    }

    #[test]
    fn new_folder_refuses_a_parent_the_tablet_does_not_show() {
        let lib = library();
        // Absent, a document, in the trash, and below a trashed folder.
        assert!(check_new_folder(&lib, "X", Some("gone")).is_err());
        assert!(check_new_folder(&lib, "X", Some("note")).is_err());
        assert!(check_new_folder(&lib, "X", Some("old")).is_err());
        assert!(check_new_folder(&lib, "X", Some("low")).is_err());
    }

    #[test]
    fn new_folder_refuses_a_parent_chain_that_loops() {
        let lib = listing(vec![
            item("a", "A", Some("b"), FOLDER),
            item("b", "B", Some("a"), FOLDER),
        ]);
        let err = check_new_folder(&lib, "X", Some("a")).unwrap_err();
        assert!(err.to_string().contains("loops"));
    }

    #[test]
    fn new_folder_refuses_a_name_that_is_not_one_plain_name() {
        let lib = library();
        for bad in ["", " Lead", "Trail ", "Tab\there", "Work/2026"] {
            assert!(check_new_folder(&lib, bad, None).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn new_folder_treats_soft_deleted_items_as_absent() {
        let mut dead = item("dead", "Dead", None, FOLDER);
        dead.deleted = true;
        let mut ghost = item("ghost", "Work", None, FOLDER);
        ghost.deleted = true;
        let lib = listing(vec![dead, ghost]);
        // A deleted folder is no parent, and a deleted item holds no name.
        assert!(check_new_folder(&lib, "X", Some("dead")).is_err());
        assert!(check_new_folder(&lib, "Work", None).is_ok());
    }

    #[test]
    fn new_folder_refuses_an_incomplete_listing() {
        let mut lib = library();
        lib.skipped.push(SkippedEntry {
            id: "s1".into(),
            error: "timeout".into(),
        });
        let err = check_new_folder(&lib, "Meeting Notes Q3", None).unwrap_err();
        assert!(err.to_string().contains("INCOMPLETE"));
    }
}
