//! `rr inspect`: read-only discovery of how one document or folder is
//! stored in the cloud. It only downloads: the root index (the current
//! one, or an older one named by hash), the item's own index, and the
//! item's `.metadata` and `.content` blobs. Nothing in this file uploads
//! a blob or moves the root pointer, and a test below keeps it that way.

use crate::error::{Error, Result};

use super::{parse_index, IndexEntry, SyncClient};

/// One JSON blob of the inspected item, body included.
#[derive(Debug)]
pub struct InspectedBlob {
    pub name: String,
    pub hash: String,
    pub body: String,
}

/// Everything `rr inspect` prints.
#[derive(Debug)]
pub struct Inspection {
    pub root_hash: String,
    /// `None` when an older root was named: a generation belongs to the
    /// current root pointer only.
    pub generation: Option<i64>,
    pub root_entries: usize,
    /// The item's line in the root index, exactly as the server sent it.
    pub root_line: String,
    pub doc_index_hash: String,
    pub doc_index_body: String,
    pub blobs: Vec<InspectedBlob>,
}

impl SyncClient {
    /// Read how `id` is stored under the current root, or under the older
    /// root `root_hash`. Both indexes go through `parse_index`, so a line
    /// that cannot be read stops the command as it would stop a push.
    pub async fn inspect(&self, id: &str, root_hash: Option<&str>) -> Result<Inspection> {
        let (root_hash, generation, entries) = match root_hash {
            Some(hash) => {
                let body = self.get_blob(hash, "root.docSchema").await?;
                let (_schema, entries) = parse_index(&body)?;
                (hash.to_string(), None, entries)
            }
            None => {
                let root = self.load_root().await?;
                (root.root_hash, Some(root.generation), root.entries)
            }
        };
        let entry = find_entry(&entries, id)?;
        let index_name = format!("{id}.docSchema");
        let index_body = self.get_blob(&entry.hash, &index_name).await?;
        let (_schema, doc_entries) = parse_index(&index_body)?;

        let mut blobs = Vec::new();
        for blob in json_entries(&doc_entries) {
            let body = self.get_blob(&blob.hash, &blob.id).await?;
            blobs.push(InspectedBlob {
                name: blob.id.clone(),
                hash: blob.hash.clone(),
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        Ok(Inspection {
            root_hash,
            generation,
            root_entries: entries.len(),
            root_line: entry.raw.clone().unwrap_or_default(),
            doc_index_hash: entry.hash.clone(),
            doc_index_body: String::from_utf8_lossy(&index_body).into_owned(),
            blobs,
        })
    }
}

/// The root entry for `id`, or an error that names the id.
fn find_entry<'a>(entries: &'a [IndexEntry], id: &str) -> Result<&'a IndexEntry> {
    for entry in entries {
        if entry.id == id {
            return Ok(entry);
        }
    }
    Err(Error::Other(format!("id {id} is not in that root")))
}

/// The entries whose bodies are small JSON documents worth printing. Page
/// data, thumbnails and `.rm` stroke files are left alone.
fn json_entries(doc_entries: &[IndexEntry]) -> Vec<&IndexEntry> {
    let mut out = Vec::new();
    for entry in doc_entries {
        if is_json_blob(&entry.id) {
            out.push(entry);
        }
    }
    out
}

fn is_json_blob(name: &str) -> bool {
    name.ends_with(".metadata") || name.ends_with(".content")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> IndexEntry {
        IndexEntry {
            hash: "ab".repeat(32),
            id: id.to_string(),
            subfiles: 0,
            size: 1,
            raw: None,
        }
    }

    #[test]
    fn find_entry_returns_the_matching_id() {
        let entries = vec![entry("doc-one"), entry("doc-two")];
        let found = find_entry(&entries, "doc-two").unwrap();
        assert_eq!(found.id, "doc-two");
    }

    #[test]
    fn find_entry_refuses_an_unknown_id() {
        let entries = vec![entry("doc-one")];
        let err = find_entry(&entries, "zz-9").unwrap_err();
        assert!(err.to_string().contains("zz-9"));
    }

    #[test]
    fn json_entries_keeps_metadata_and_content_only() {
        let doc = vec![
            entry("d.content"),
            entry("d.metadata"),
            entry("d.pagedata"),
            entry("d/page-one.rm"),
        ];
        let kept = json_entries(&doc);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].id, "d.content");
        assert_eq!(kept[1].id, "d.metadata");
    }

    #[test]
    fn inspect_source_only_calls_the_two_read_methods() {
        // Whatever this file reaches through the client must be one of
        // the two read calls. A field access such as the HTTP client, or
        // a method under any other name, fails here. The needle is built
        // at run time, and no comment in this file may spell it out, so
        // that the test does not trip on its own text.
        let src = include_str!("inspect.rs");
        let needle = ["self", "."].concat();
        let mut names = Vec::new();
        for piece in src.split(needle.as_str()).skip(1) {
            let name: String = piece
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            names.push(name);
        }
        assert_eq!(names.len(), 4);
        for name in &names {
            let allowed = matches!(name.as_str(), "load_root" | "get_blob");
            assert!(allowed, "inspect.rs reaches the client through {name}");
        }
    }

    #[test]
    fn inspect_source_has_no_write_call() {
        // The needles are assembled at run time so that this test does not
        // trip on its own text.
        let src = include_str!("inspect.rs");
        for verb in ["put", "post", "patch", "delete"] {
            let call = [".", verb, "("].concat();
            assert!(!src.contains(&call), "inspect.rs calls {call}");
        }
        let writers = [
            "put_blob",
            "put_to_signed_url",
            "update_root",
            "upload_bundle",
            "apply_restore",
        ];
        for name in writers {
            let call = [name, "("].concat();
            assert!(!src.contains(&call), "inspect.rs calls {call}");
        }
    }
}
