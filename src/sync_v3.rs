//! reMarkable cloud sync v3 — direct native-bundle upload.
//!
//! This is the protocol every reMarkable device speaks to
//! `internal.cloud.remarkable.com`. It's free with any reMarkable account
//! — Connect is a separate paid tier that adds storage and templates, but
//! the core sync API is open to everyone. Uploading a notebook this way
//! ships the real binary `.rm` files we built in Phase 1-3 directly, with
//! no SSH, no USB cable, and no cloud-side EPUB conversion.
//!
//! ## Protocol overview
//!
//! Files are stored as content-addressed blobs keyed by SHA-256. To add a
//! new document:
//!
//! 1. `PUT /sync/v3/files/<sha256>` for every file in the bundle.
//! 2. Build a *doc-index* blob: a text listing of `<file-hash>:0:<name>:0:<size>`
//!    lines, sorted by name. `PUT` it under its own hash.
//! 3. `GET /sync/v3/root` → `{ hash, generation }`. Fetch and parse the
//!    *root index* blob at `hash` — it's the same line format but lists
//!    every document.
//! 4. Replace-or-append our doc's entry, serialize, hash, `PUT` the new
//!    root blob.
//! 5. `PUT /sync/v3/root` with `{ hash, generation, broadcast: true }`.
//!    Server compares `generation` to its stored value; on a race it
//!    returns 412 and we retry from step 3.
//!
//! ## Schema versions
//!
//! Indexes carry their schema on the first line (`3` or `4`).
//! - v4 hashes are `sha256(blob_bytes)`.
//! - v3 hashes are `sha256(concat(binary_child_hashes))` sorted by id.
//! - v4 indexes include a `0:<label>:<count>:<totalSize>` totals row right
//!   after the schema line; v3 does not.
//!
//! We always honour the schema the server is currently using.

use base64::Engine;
use futures::stream::{self, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::notebook::Bundle;

const SYNC_HOST: &str = "https://internal.cloud.remarkable.com";
const SIGNED_UPLOAD_HOST_SUFFIXES: &[&str] = &[".googleapis.com", ".amazonaws.com"];

fn root_url() -> String {
    format!("{SYNC_HOST}/sync/v3/root")
}

fn files_url(hash: &str) -> String {
    format!("{SYNC_HOST}/sync/v3/files/{hash}")
}

/// Index schema identifiers used by the cloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schema {
    V3,
    V4,
}

impl Schema {
    fn as_str(self) -> &'static str {
        match self {
            Schema::V3 => "3",
            Schema::V4 => "4",
        }
    }

    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "3" => Ok(Schema::V3),
            "4" => Ok(Schema::V4),
            other => Err(Error::InvalidResponse(format!(
                "unknown index schema {other:?}"
            ))),
        }
    }
}

/// One line of an index blob: a child hash plus the friendly name it's
/// stored under. For doc-indexes `id` is the filename
/// (e.g. `<doc>.metadata`); for the root index it's the docID (UUID).
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub hash: String,
    pub id: String,
    pub subfiles: u32,
    pub size: u64,
    /// The exact line the server sent for this entry. Set only by
    /// `parse_index`; `serialize_index` re-emits it byte-for-byte, so
    /// rewriting an index never reinterprets an entry we did not create.
    /// `None` for entries built locally.
    pub raw: Option<String>,
}

#[derive(Debug)]
pub struct RootState {
    pub schema: Schema,
    pub root_hash: String,
    pub generation: i64,
    pub entries: Vec<IndexEntry>,
    /// The root index blob exactly as the server returned it. Kept so a
    /// dry run can diff the rewritten index against the real one.
    pub body: String,
}

/// Async client for the sync v3 endpoints.
pub struct SyncClient {
    http: reqwest::Client,
    token: String,
}

impl SyncClient {
    pub fn new(token: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .connect_timeout(std::time::Duration::from_secs(10))
            .user_agent(concat!("rr/", env!("CARGO_PKG_VERSION")))
            // The sync API doesn't 30x in practice, but keep manual redirect
            // handling so a GCS signed-URL bounce wouldn't carry our custom
            // `rm-*` headers along — those would confuse GCS.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(Error::Network)?;
        Ok(Self {
            http,
            token: token.into(),
        })
    }

    fn auth_headers(&self) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.token))
                .map_err(|e| Error::Config(format!("bad bearer header: {e}")))?,
        );
        Ok(h)
    }

    /// `GET /sync/v3/root` and fetch the root index blob it points at.
    pub async fn load_root(&self) -> Result<RootState> {
        let url = root_url();
        tracing::debug!(method = "GET", url = %url, "sync_v3 load_root");
        let resp = self
            .http
            .get(&url)
            .headers(self.auth_headers()?)
            .send()
            .await
            .map_err(Error::Network)?;
        let status = resp.status();
        tracing::debug!(status = %status, "sync_v3 load_root response");
        let body = resp.bytes().await.map_err(Error::Network)?;
        if !status.is_success() {
            return Err(Error::Api {
                status: status.as_u16(),
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        #[derive(Deserialize)]
        struct RootPointer {
            hash: String,
            generation: i64,
        }
        let p: RootPointer = serde_json::from_slice(&body)?;
        tracing::debug!(
            root_hash = %p.hash,
            generation = p.generation,
            "sync_v3 root pointer"
        );

        let index_body = self.get_blob(&p.hash, "root.docSchema").await?;
        let (schema, entries) = parse_index(&index_body)?;
        Ok(RootState {
            schema,
            root_hash: p.hash,
            generation: p.generation,
            entries,
            body: String::from_utf8_lossy(&index_body).into_owned(),
        })
    }

    /// Enumerate every document/folder visible to this token by walking
    /// the root index and pulling each entry's `.metadata` blob.
    ///
    /// This is the listing path that works for **any** reMarkable account —
    /// the higher-level `/doc/v2/files` endpoint requires Connect-tier
    /// scopes, but `/sync/v3/*` is open to every paired device. We pay one
    /// `load_root` plus two GETs per document (the doc-index, then the
    /// `.metadata` blob inside it). Per-doc work is fanned out with a
    /// bounded concurrency so a 200-document library still completes in a
    /// few seconds without hammering the server.
    ///
    /// An entry whose metadata cannot be read is never dropped silently:
    /// it comes back in [`Listing::skipped`] so the caller has to say so.
    pub async fn list_documents(&self) -> Result<Listing> {
        const FETCH_CONCURRENCY: usize = 16;

        let root = self.load_root().await?;
        let results = stream::iter(root.entries)
            .map(|entry| async move {
                let id = entry.id.clone();
                // One retry, and only for a retryable error: the omission
                // seen live was a transient fetch on a slow link, and
                // nothing else on this read path retries.
                let mut fetched = self.fetch_document_info(&entry).await;
                let retry = match &fetched {
                    Err(e) => e.is_retryable(),
                    Ok(_) => false,
                };
                if retry {
                    fetched = self.fetch_document_info(&entry).await;
                }
                fetched.map_err(|e| {
                    tracing::warn!(doc = %id, error = %e, "skip doc: metadata fetch failed");
                    SkippedEntry {
                        id,
                        error: e.to_string(),
                    }
                })
            })
            .buffer_unordered(FETCH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;

        Ok(partition_listing(results))
    }

    /// For one root entry: GET its `.docSchema` index, find the entry whose
    /// `id` ends in `.metadata`, GET that blob, parse out visible name,
    /// type, and parent.
    async fn fetch_document_info(&self, entry: &IndexEntry) -> Result<DocumentInfo> {
        let doc_id = entry.id.clone();
        let doc_index_body = self
            .get_blob(&entry.hash, &format!("{doc_id}.docSchema"))
            .await?;
        let (_schema, doc_entries) = parse_index(&doc_index_body)?;

        let meta_entry = doc_entries
            .iter()
            .find(|e| e.id.ends_with(".metadata"))
            .ok_or_else(|| {
                Error::InvalidResponse(format!("doc {doc_id} has no .metadata entry"))
            })?;

        let meta_bytes = self.get_blob(&meta_entry.hash, &meta_entry.id).await?;
        let meta: DocMetaBlob = serde_json::from_slice(&meta_bytes)
            .map_err(|e| Error::InvalidResponse(format!("decode metadata for {doc_id}: {e}")))?;

        // The cloud stores soft-deletes with `deleted: true` but still keeps
        // them in the root index for a while. Surface them as a separate
        // field so callers can filter (we hide them in `rr ls`).
        Ok(DocumentInfo {
            id: doc_id,
            visible_name: meta.visible_name.unwrap_or_default(),
            doc_type: meta.type_field.unwrap_or_default(),
            parent: meta.parent.filter(|p| !p.is_empty()),
            deleted: meta.deleted.unwrap_or(false),
        })
    }

    /// Fetch a blob by its content hash.
    ///
    /// The server REQUIRES an `rm-filename` header on GETs to
    /// `/sync/v3/files/<hash>`, even though the blob is content-addressed
    /// and the name is just a routing hint. The 400 response when the
    /// header is missing says "unexpected 'rm-filename' http header" —
    /// confusingly, this is the API's way of saying it expected the
    /// header and didn't get it. Pass the friendly name the parent index
    /// stored this hash under (e.g. `root.docSchema`, `<docID>.docSchema`,
    /// `<docID>.metadata`).
    pub async fn get_blob(&self, hash: &str, rm_filename: &str) -> Result<Vec<u8>> {
        let url = files_url(hash);
        tracing::debug!(method = "GET", url = %url, rm_filename = rm_filename, "sync_v3 blob fetch");
        let mut headers = self.auth_headers()?;
        headers.insert(
            HeaderName::from_static("rm-filename"),
            HeaderValue::from_str(rm_filename)
                .map_err(|e| Error::Config(format!("bad rm-filename: {e}")))?,
        );
        let resp = self
            .http
            .get(&url)
            .headers(headers)
            .send()
            .await
            .map_err(Error::Network)?;
        let status = resp.status();
        tracing::debug!(status = %status, "sync_v3 blob fetch response");
        let body = resp.bytes().await.map_err(Error::Network)?;
        if !status.is_success() {
            return Err(Error::Api {
                status: status.as_u16(),
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        Ok(body.to_vec())
    }

    /// `PUT /sync/v3/files/<hash>` — upload a single content-addressed
    /// blob. Carries the friendly filename, byte count, and a CRC32C hash
    /// in lowercase headers the server explicitly checks for.
    pub async fn put_blob(&self, hash: &str, data: &[u8], rm_filename: &str) -> Result<()> {
        let mut headers = self.auth_headers()?;
        // These three headers must be lowercase on the wire — the server
        // canonicalises lookups against the literal byte sequence. reqwest's
        // HeaderName::from_static stores lowercase by construction so we're
        // safe using it directly.
        headers.insert(
            HeaderName::from_static("rm-filename"),
            HeaderValue::from_str(rm_filename)
                .map_err(|e| Error::Config(format!("bad rm-filename: {e}")))?,
        );
        headers.insert(
            HeaderName::from_static("rm-filesize"),
            HeaderValue::from_str(&data.len().to_string())
                .map_err(|e| Error::Config(format!("bad rm-filesize: {e}")))?,
        );
        headers.insert(
            HeaderName::from_static("x-goog-hash"),
            HeaderValue::from_str(&format!("crc32c={}", crc32c_base64(data)))
                .map_err(|e| Error::Config(format!("bad x-goog-hash: {e}")))?,
        );

        let url = files_url(hash);
        tracing::debug!(
            method = "PUT",
            url = %url,
            rm_filename = rm_filename,
            rm_filesize = data.len(),
            "sync_v3 blob upload"
        );
        let resp = self
            .http
            .put(&url)
            .headers(headers)
            .body(data.to_vec())
            .send()
            .await
            .map_err(Error::Network)?;
        let status = resp.status();
        tracing::debug!(status = %status, "sync_v3 blob upload response");

        // 3xx — the sync server is handing us a signed URL (typically GCS).
        // PUT the bytes to that URL with only the headers it accepts.
        if status.is_redirection() {
            if let Some(location) = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
            {
                tracing::debug!(redirect = %location, "following signed-URL redirect");
                return self.put_to_signed_url(&location, data).await;
            }
            let body = resp.bytes().await.map_err(Error::Network)?;
            return Err(Error::Api {
                status: status.as_u16(),
                body: format!(
                    "redirect without Location header: {}",
                    String::from_utf8_lossy(&body).trim()
                ),
            });
        }

        if status.is_success() {
            return Ok(());
        }
        let body = resp.bytes().await.map_err(Error::Network)?;
        Err(Error::Api {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }

    /// Upload `data` to a server-supplied signed URL (GCS / S3 style).
    /// These URLs reject our `rm-*` headers, so we send only `x-goog-hash`
    /// for CRC32C verification when the URL is GCS.
    async fn put_to_signed_url(&self, url: &str, data: &[u8]) -> Result<()> {
        let url = validate_signed_upload_url(url)?;
        let mut req = self.http.put(url.clone()).body(data.to_vec());
        // GCS uses x-goog-hash and respects nothing else from our custom
        // header set. S3-style signed URLs (if reMarkable ever switches)
        // tolerate it being absent.
        if url
            .host_str()
            .is_some_and(|host| host.ends_with(".googleapis.com"))
        {
            req = req.header("x-goog-hash", format!("crc32c={}", crc32c_base64(data)));
        }
        let resp = req.send().await.map_err(Error::Network)?;
        let status = resp.status();
        tracing::debug!(status = %status, "signed-URL upload response");
        if status.is_success() {
            return Ok(());
        }
        let body = resp.bytes().await.map_err(Error::Network)?;
        Err(Error::Api {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }

    /// `PUT /sync/v3/root` with the current generation. On 412 the caller
    /// is expected to re-fetch root and retry.
    pub async fn update_root(
        &self,
        new_hash: &str,
        current_generation: i64,
    ) -> Result<UpdateRootOutcome> {
        let mut headers = self.auth_headers()?;
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            HeaderName::from_static("rm-filename"),
            HeaderValue::from_static("roothash"),
        );

        #[derive(Serialize)]
        struct RootUpdateReq<'a> {
            broadcast: bool,
            hash: &'a str,
            generation: i64,
        }
        let body = serde_json::to_vec(&RootUpdateReq {
            broadcast: true,
            hash: new_hash,
            generation: current_generation,
        })?;

        let resp = self
            .http
            .put(root_url())
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(Error::Network)?;
        let status = resp.status();
        if status == reqwest::StatusCode::PRECONDITION_FAILED {
            return Ok(UpdateRootOutcome::GenerationRace);
        }
        let body = resp.bytes().await.map_err(Error::Network)?;
        if !status.is_success() {
            return Err(Error::Api {
                status: status.as_u16(),
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            generation: i64,
        }
        let r: Resp = serde_json::from_slice(&body).unwrap_or(Resp {
            generation: current_generation + 1,
        });
        Ok(UpdateRootOutcome::Updated {
            new_generation: r.generation,
        })
    }

    /// Upload an entire notebook bundle to the cloud as a new document.
    /// Atomically attaches it to the root index via optimistic-concurrency
    /// retries.
    pub async fn upload_bundle(&self, bundle: &Bundle) -> Result<UploadResult> {
        // 1. Snapshot files we need to push.
        let doc_uuid = bundle.doc_uuid.to_string();
        let files = bundle_files(bundle);

        // 2. PUT every blob with its rm-filename.
        let mut doc_entries: Vec<IndexEntry> = Vec::with_capacity(files.len());
        for f in &files {
            let entry = entry_for(f);
            self.put_blob(&entry.hash, &f.bytes, &f.cloud_name).await?;
            doc_entries.push(entry);
        }

        // 3. Up to a few retries on the root-generation race.
        const MAX_ATTEMPTS: usize = 3;
        for attempt in 0..MAX_ATTEMPTS {
            let root = self.load_root().await?;

            // The whole index layer comes from `plan_root`, the same pure
            // function the dry run uses, and is computed (invariant check
            // included) before any of it is sent.
            let planned = plan_root(&root, &doc_uuid, &doc_entries)?;
            let (doc_body, root_body) = (planned.doc_body, planned.root_body);
            let (doc_index_hash, new_root_hash) = (planned.doc_index_hash, planned.root_hash);

            self.put_blob(&doc_index_hash, &doc_body, &format!("{doc_uuid}.docSchema"))
                .await?;
            self.put_blob(&new_root_hash, &root_body, "root.docSchema")
                .await?;

            // Atomically swap the root pointer.
            match self.update_root(&new_root_hash, root.generation).await? {
                UpdateRootOutcome::Updated { new_generation } => {
                    return Ok(UploadResult {
                        doc_id: doc_uuid,
                        doc_index_hash,
                        new_root_hash,
                        new_generation,
                        previous_root_hash: root.root_hash.clone(),
                        previous_generation: root.generation,
                    });
                }
                UpdateRootOutcome::GenerationRace if attempt + 1 < MAX_ATTEMPTS => {
                    continue;
                }
                UpdateRootOutcome::GenerationRace => {
                    return Err(Error::Other(
                        "root generation race after 3 retries — try again".into(),
                    ));
                }
            }
        }
        unreachable!()
    }

    /// Dry run of `upload_bundle`: fetch the current root (two GETs),
    /// compute exactly what a push would upload, and report how the
    /// rewritten root index differs from the real one. Sends nothing.
    pub async fn plan_bundle(&self, bundle: &Bundle) -> Result<PushPlan> {
        let doc_uuid = bundle.doc_uuid.to_string();
        let files = bundle_files(bundle);
        let doc_entries: Vec<IndexEntry> = files.iter().map(entry_for).collect();

        let root = self.load_root().await?;
        let planned = plan_root(&root, &doc_uuid, &doc_entries)?;
        let new_body = String::from_utf8_lossy(&planned.root_body).into_owned();
        Ok(PushPlan {
            doc_id: doc_uuid,
            blobs_to_upload: files.len() + 2,
            previous_root_hash: root.root_hash.clone(),
            previous_generation: root.generation,
            new_root_hash: planned.root_hash,
            diff: diff_root(root.schema, &root.body, &new_body),
        })
    }

    /// Plan a rollback of the root pointer to `target_hash`. Reads only:
    /// the current root, and the target index blob, which must still exist
    /// and must parse under the same fail-closed rules. Writes nothing.
    pub async fn plan_restore(&self, target_hash: &str) -> Result<RestorePlan> {
        let current = self.load_root().await?;
        let target_body = self.get_blob(target_hash, "root.docSchema").await?;
        let (target_schema, target_entries) = parse_index(&target_body)?;
        let target_text = String::from_utf8_lossy(&target_body).into_owned();
        Ok(RestorePlan {
            current_root_hash: current.root_hash.clone(),
            current_generation: current.generation,
            current_entries: current.entries.len(),
            target_root_hash: target_hash.to_string(),
            target_entries: target_entries.len(),
            schema_changed: target_schema != current.schema,
            diff: diff_root(current.schema, &current.body, &target_text),
        })
    }

    /// Point the root at the plan's target. Uses the same guarded swap as
    /// a push: if the root moved after the plan was made, the server
    /// answers 412 and nothing changes.
    pub async fn apply_restore(&self, plan: &RestorePlan) -> Result<i64> {
        let target = &plan.target_root_hash;
        let outcome = self.update_root(target, plan.current_generation).await?;
        if let UpdateRootOutcome::Updated { new_generation } = outcome {
            return Ok(new_generation);
        }
        Err(Error::Other(
            "root changed since the plan was made; re-run root-restore".into(),
        ))
    }
}

/// Index-layer output of a push, computed without touching the network.
/// `upload_bundle` and `plan_bundle` both get it from `plan_root`, so a dry
/// run shows exactly what a real push would upload.
struct NewRoot {
    doc_body: Vec<u8>,
    doc_index_hash: String,
    root_body: Vec<u8>,
    root_hash: String,
}

/// Build the document index and the rewritten root index for one new
/// document. Pure: no I/O.
fn plan_root(root: &RootState, doc_uuid: &str, doc_entries: &[IndexEntry]) -> Result<NewRoot> {
    let doc_body = serialize_index(root.schema, doc_uuid, doc_entries, false);
    let doc_index_hash = hash_index(root.schema, doc_entries, &doc_body)?;

    let total_size: u64 = doc_entries.iter().map(|e| e.size).sum();
    let new_entry = IndexEntry {
        hash: doc_index_hash.clone(),
        id: doc_uuid.to_string(),
        subfiles: doc_entries.len() as u32,
        size: total_size,
        raw: None,
    };
    let entries = replace_or_append(root.entries.clone(), new_entry);

    // Invariant: a push adds exactly one document, or replaces our own id
    // on a retry. Any other count means the rewritten root would lose or
    // duplicate entries, so stop before anything is sent.
    let already_present = root.entries.iter().any(|e| e.id == doc_uuid);
    let expected_len = root.entries.len() + usize::from(!already_present);
    if entries.len() != expected_len {
        return Err(Error::Other(format!(
            "root rewrite would change entry count {} -> {} (expected {}); aborted",
            root.entries.len(),
            entries.len(),
            expected_len
        )));
    }

    let root_body = serialize_index(root.schema, ".", &entries, true);
    let root_hash = hash_index(root.schema, &entries, &root_body)?;
    Ok(NewRoot {
        doc_body,
        doc_index_hash,
        root_body,
        root_hash,
    })
}

/// Index entry for one blob of a document: content hash, name and size.
fn entry_for(f: &CloudFile) -> IndexEntry {
    IndexEntry {
        hash: sha256_hex(&f.bytes),
        id: f.cloud_name.clone(),
        subfiles: 0,
        size: f.bytes.len() as u64,
        raw: None,
    }
}

/// What a push would do, as reported by `SyncClient::plan_bundle`.
#[derive(Debug, Clone)]
pub struct PushPlan {
    pub doc_id: String,
    /// Document blobs, plus the document index, plus the new root index.
    pub blobs_to_upload: usize,
    pub previous_root_hash: String,
    pub previous_generation: i64,
    pub new_root_hash: String,
    pub diff: RootDiff,
}

/// What `rr root-restore` would do, as reported by `plan_restore`.
#[derive(Debug, Clone)]
pub struct RestorePlan {
    pub current_root_hash: String,
    pub current_generation: i64,
    pub current_entries: usize,
    pub target_root_hash: String,
    pub target_entries: usize,
    pub schema_changed: bool,
    /// Current index compared with the target: `removed` lines would
    /// disappear from the library, `added` lines would come back.
    pub diff: RootDiff,
}

/// Line-level comparison of the root index before and after a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootDiff {
    pub old_totals: Option<String>,
    pub new_totals: Option<String>,
    pub old_entries: usize,
    pub new_entries: usize,
    /// Entry lines present before and missing after. Must be empty.
    pub removed: Vec<String>,
    /// Entry lines present after and not before. Exactly one for a push.
    pub added: Vec<String>,
    /// True when the new index, with `added` taken out, lists the old
    /// entry lines in the same order, byte for byte.
    pub order_preserved: bool,
}

fn is_totals_row(line: &str) -> bool {
    let parts: Vec<&str> = line.split(':').collect();
    parts.len() == 4 && parts[0] == "0"
}

/// Split an index body into its optional v4 totals row and its entry lines.
fn split_index(schema: Schema, body: &str) -> (Option<String>, Vec<String>) {
    let mut rest: Vec<String> = body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .skip(1)
        .map(str::to_owned)
        .collect();
    let mut totals = None;
    if schema == Schema::V4 && rest.first().is_some_and(|l| is_totals_row(l)) {
        totals = Some(rest.remove(0));
    }
    (totals, rest)
}

/// Lines of `a` that do not appear in `b`, in `a`'s order.
fn lines_missing_from(a: &[String], b: &[String]) -> Vec<String> {
    a.iter().filter(|l| !b.contains(*l)).cloned().collect()
}

fn diff_root(schema: Schema, old_body: &str, new_body: &str) -> RootDiff {
    let (old_totals, old_lines) = split_index(schema, old_body);
    let (new_totals, new_lines) = split_index(schema, new_body);

    let removed = lines_missing_from(&old_lines, &new_lines);
    let added = lines_missing_from(&new_lines, &old_lines);
    let kept = lines_missing_from(&new_lines, &added);
    let order_preserved = kept == old_lines;

    RootDiff {
        old_totals,
        new_totals,
        old_entries: old_lines.len(),
        new_entries: new_lines.len(),
        removed,
        added,
        order_preserved,
    }
}

fn validate_signed_upload_url(raw: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw)
        .map_err(|e| Error::InvalidResponse(format!("invalid signed upload URL: {e}")))?;
    if url.scheme() != "https" {
        return Err(Error::InvalidResponse(
            "signed upload URL must use HTTPS".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(Error::InvalidResponse(
            "signed upload URL contains forbidden credentials or fragment".into(),
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| Error::InvalidResponse("signed upload URL has no host".into()))?;
    if !SIGNED_UPLOAD_HOST_SUFFIXES
        .iter()
        .any(|suffix| host.ends_with(suffix))
    {
        return Err(Error::InvalidResponse(format!(
            "signed upload URL host is not approved: {host}"
        )));
    }
    Ok(url)
}

/// What the root-update PUT returned.
#[derive(Debug)]
pub enum UpdateRootOutcome {
    Updated { new_generation: i64 },
    GenerationRace,
}

/// One row returned by [`SyncClient::list_documents`]. Shaped to match the
/// fields `rr ls` and the deprecated `cloud_api::FileItem` consumed, so
/// the CLI surface doesn't have to care which endpoint the data came from.
#[derive(Debug, Clone)]
pub struct DocumentInfo {
    pub id: String,
    pub visible_name: String,
    /// Either `"DocumentType"` or `"CollectionType"` (folder), matching the
    /// device-side metadata format.
    pub doc_type: String,
    pub parent: Option<String>,
    pub deleted: bool,
}

impl DocumentInfo {
    pub fn is_folder(&self) -> bool {
        self.doc_type == "CollectionType"
    }
}

/// What [`SyncClient::list_documents`] could and could not read.
///
/// A listing that hides an entry can read as data loss, or mask it, so
/// `skipped` travels with the documents and callers must surface it.
#[derive(Debug, Clone)]
pub struct Listing {
    pub docs: Vec<DocumentInfo>,
    pub skipped: Vec<SkippedEntry>,
}

/// A root entry whose metadata could not be fetched or decoded.
#[derive(Debug, Clone)]
pub struct SkippedEntry {
    pub id: String,
    pub error: String,
}

impl Listing {
    /// `None` when every root entry was read. Otherwise the text `rr ls`
    /// prints: how many entries are missing from the listing, and which.
    pub fn incomplete_summary(&self) -> Option<String> {
        if self.skipped.is_empty() {
            return None;
        }
        let k = self.skipped.len();
        let total = self.docs.len() + k;
        let mut out = format!("INCOMPLETE: {k} of {total} root entries");
        out.push_str(" could not be read and are not shown:");
        for s in &self.skipped {
            out.push_str(&format!("\n  {}  ({})", s.id, s.error));
        }
        Some(out)
    }
}

type FetchOutcome = std::result::Result<DocumentInfo, SkippedEntry>;

/// Split per-entry results into readable documents and skipped entries.
/// Documents sort folders first, then by name; skipped entries sort by id,
/// so the output is the same whatever order the fetches finished in.
fn partition_listing(results: Vec<FetchOutcome>) -> Listing {
    let mut docs = Vec::new();
    let mut skipped = Vec::new();
    for r in results {
        match r {
            Ok(d) => docs.push(d),
            Err(s) => skipped.push(s),
        }
    }
    // Folders first, then documents, alphabetised within each group.
    // This matches what `rr ls` users have grown used to and keeps
    // output deterministic for golden tests.
    docs.sort_by(|a, b| {
        let ord = b.is_folder().cmp(&a.is_folder());
        if ord != std::cmp::Ordering::Equal {
            ord
        } else {
            a.visible_name
                .to_lowercase()
                .cmp(&b.visible_name.to_lowercase())
        }
    });
    skipped.sort_by(|a, b| a.id.cmp(&b.id));
    Listing { docs, skipped }
}

/// Minimal view of a `.metadata` blob — only the fields `rr ls` needs.
/// Every field is optional because old documents on the cloud predate the
/// current schema and may omit pieces.
#[derive(Debug, Deserialize)]
struct DocMetaBlob {
    #[serde(rename = "visibleName", default)]
    visible_name: Option<String>,
    #[serde(rename = "type", default)]
    type_field: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    deleted: Option<bool>,
}

/// Summary returned by [`SyncClient::upload_bundle`].
#[derive(Debug, Clone)]
pub struct UploadResult {
    pub doc_id: String,
    pub doc_index_hash: String,
    pub new_root_hash: String,
    pub new_generation: i64,
    /// Root pointer as it stood immediately before this upload swapped it.
    /// Blobs are content-addressed and never overwritten, so this hash is
    /// the handle for restoring the prior library state.
    pub previous_root_hash: String,
    pub previous_generation: i64,
}

/// One file the cloud needs: bytes plus the friendly filename the server
/// stamps the blob with via `rm-filename` and the doc index references it
/// by.
struct CloudFile {
    cloud_name: String,
    bytes: Vec<u8>,
}

/// Flatten a [`Bundle`] into the per-file list the cloud expects.
fn bundle_files(bundle: &Bundle) -> Vec<CloudFile> {
    let doc = bundle.doc_uuid.to_string();
    let mut out = Vec::with_capacity(3 + bundle.pages.len() * 2);
    out.push(CloudFile {
        cloud_name: format!("{doc}.metadata"),
        bytes: bundle.metadata_json.clone().into_bytes(),
    });
    out.push(CloudFile {
        cloud_name: format!("{doc}.content"),
        bytes: bundle.content_json.clone().into_bytes(),
    });
    out.push(CloudFile {
        cloud_name: format!("{doc}.pagedata"),
        bytes: bundle.pagedata.clone().into_bytes(),
    });
    for page in &bundle.pages {
        let pid = page.uuid.to_string();
        out.push(CloudFile {
            cloud_name: format!("{doc}/{pid}.rm"),
            bytes: page.rm_bytes.clone(),
        });
        out.push(CloudFile {
            cloud_name: format!("{doc}/{pid}-metadata.json"),
            bytes: page.metadata_json.clone().into_bytes(),
        });
        // Per-image PNG attachments live at
        // `<doc>/<page-uuid>/<image-uuid>.png` and ship as additional
        // blobs in the doc-index so the device knows to fetch them.
        for img in &page.images {
            out.push(CloudFile {
                cloud_name: format!("{doc}/{pid}/{}", img.filename),
                bytes: img.png_bytes.clone(),
            });
        }
    }
    out
}

/// Insert `entry` into `entries`, replacing any existing entry with the
/// same `id`. Returns a new vector — callers want the original preserved.
fn replace_or_append(mut entries: Vec<IndexEntry>, entry: IndexEntry) -> Vec<IndexEntry> {
    if let Some(slot) = entries.iter_mut().find(|e| e.id == entry.id) {
        *slot = entry;
    } else {
        entries.push(entry);
    }
    entries
}

/// Parse a root or doc-index blob body. Format:
/// - Line 1: schema id (`3` or `4`).
/// - v4 only: optional totals line `0:<label>:<count>:<totalSize>`.
/// - Each subsequent line: `<hash>:<type>:<id>:<subfiles>:<size>`.
fn parse_index(body: &[u8]) -> Result<(Schema, Vec<IndexEntry>)> {
    let text = std::str::from_utf8(body)
        .map_err(|e| Error::InvalidResponse(format!("index blob is not utf-8: {e}")))?;
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let schema_line = lines
        .next()
        .ok_or_else(|| Error::InvalidResponse("empty index blob".into()))?;
    let schema = Schema::from_str(schema_line)?;

    let mut entries = Vec::new();
    let mut first_after_schema = true;
    for line in lines {
        // v4 totals row: "0:<label>:<count>:<totalSize>". Detected by 4
        // colon-separated fields whose first field is exactly "0".
        if schema == Schema::V4 && first_after_schema {
            first_after_schema = false;
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() == 4 && parts[0] == "0" {
                continue;
            }
        }
        first_after_schema = false;

        entries.push(parse_entry_line(line)?);
    }
    Ok((schema, entries))
}

/// Parse one `<hash>:<type>:<id>:<subfiles>:<size>` line, failing closed.
///
/// The root index is rewritten from whatever `parse_index` returns, so a
/// line we cannot fully read has to stop the operation. Skipping it would
/// drop that document from the account-wide index on the next push, and
/// paired devices act on the index.
fn parse_entry_line(line: &str) -> Result<IndexEntry> {
    let parts: Vec<&str> = line.split(':').collect();
    if parts.len() != 5 || parts[0].is_empty() || parts[2].is_empty() {
        return Err(bad_index_line(line));
    }
    let Ok(subfiles) = parts[3].parse::<u32>() else {
        return Err(bad_index_line(line));
    };
    let Ok(size) = parts[4].parse::<u64>() else {
        return Err(bad_index_line(line));
    };
    Ok(IndexEntry {
        hash: parts[0].to_string(),
        id: parts[2].to_string(),
        subfiles,
        size,
        raw: Some(line.to_string()),
    })
}

fn bad_index_line(line: &str) -> Error {
    Error::InvalidResponse(format!(
        "index line is not <hash>:<type>:<id>:<subfiles>:<size>; refusing to continue: {line:?}"
    ))
}

/// Serialize an index blob. `label` is the docID for doc indexes or `"."`
/// for the root. `is_root` controls the per-entry "type" field, which is
/// `"80000000"` only for the v3 root and `"0"` everywhere else.
fn serialize_index(schema: Schema, label: &str, entries: &[IndexEntry], is_root: bool) -> Vec<u8> {
    let mut sorted: Vec<&IndexEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));

    let mut out = String::new();
    out.push_str(schema.as_str());
    out.push('\n');

    if schema == Schema::V4 && !label.is_empty() {
        let total: u64 = sorted.iter().map(|e| e.size).sum();
        out.push_str(&format!("0:{label}:{}:{}\n", sorted.len(), total));
    }

    let type_field = if schema == Schema::V3 && is_root {
        "80000000"
    } else {
        "0"
    };

    for e in sorted {
        // Entries that came from the server go back out exactly as they
        // arrived, including a type field this client has never seen.
        if let Some(raw) = &e.raw {
            out.push_str(raw);
            out.push('\n');
            continue;
        }
        out.push_str(&format!(
            "{}:{}:{}:{}:{}\n",
            e.hash, type_field, e.id, e.subfiles, e.size
        ));
    }
    out.into_bytes()
}

/// Compute the hash that goes into a parent (root pointer or parent
/// doc-index entry).
///
/// - v4: SHA-256 over the serialised blob bytes.
/// - v3: SHA-256 over the concatenation of the *binary-decoded* child
///   hashes, ordered by id. Yes, decoded — the server expects raw bytes
///   here, not the hex string.
fn hash_index(schema: Schema, entries: &[IndexEntry], body: &[u8]) -> Result<String> {
    match schema {
        Schema::V4 => Ok(sha256_hex(body)),
        Schema::V3 => {
            let mut sorted: Vec<&IndexEntry> = entries.iter().collect();
            sorted.sort_by(|a, b| a.id.cmp(&b.id));
            let mut hasher = Sha256::new();
            for e in sorted {
                let raw = hex::decode(&e.hash).map_err(|err| {
                    Error::InvalidResponse(format!("bad child hash {:?}: {err}", e.hash))
                })?;
                hasher.update(&raw);
            }
            Ok(hex::encode(hasher.finalize()))
        }
    }
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

fn crc32c_base64(data: &[u8]) -> String {
    let v: u32 = crc32c::crc32c(data);
    let bytes = v.to_be_bytes();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_round_trip() {
        assert_eq!(Schema::V3.as_str(), "3");
        assert_eq!(Schema::V4.as_str(), "4");
        assert!(matches!(Schema::from_str("3"), Ok(Schema::V3)));
        assert!(matches!(Schema::from_str("4"), Ok(Schema::V4)));
        assert!(Schema::from_str("5").is_err());
    }

    #[test]
    fn parse_v4_index_with_totals_row() {
        let body = b"4\n0:.:2:300\nabc:0:doc1:1:100\ndef:0:doc2:1:200\n";
        let (schema, entries) = parse_index(body).unwrap();
        assert_eq!(schema, Schema::V4);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].hash, "abc");
        assert_eq!(entries[0].id, "doc1");
        assert_eq!(entries[1].size, 200);
    }

    #[test]
    fn parse_v3_index_without_totals_row() {
        let body = b"3\nabc:80000000:doc1:1:100\ndef:80000000:doc2:1:200\n";
        let (schema, entries) = parse_index(body).unwrap();
        assert_eq!(schema, Schema::V3);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn serialize_round_trips_through_parse() {
        let entries = vec![
            IndexEntry {
                hash: "deadbeef".repeat(8),
                id: "z.rm".into(),
                subfiles: 0,
                size: 42,
                raw: None,
            },
            IndexEntry {
                hash: "cafebabe".repeat(8),
                id: "a.rm".into(),
                subfiles: 0,
                size: 7,
                raw: None,
            },
        ];
        let body = serialize_index(Schema::V4, "docid", &entries, false);
        let (schema, parsed) = parse_index(&body).unwrap();
        assert_eq!(schema, Schema::V4);
        // Entries come back sorted by id, so a.rm before z.rm.
        assert_eq!(parsed[0].id, "a.rm");
        assert_eq!(parsed[1].id, "z.rm");
    }

    #[test]
    fn replace_or_append_replaces_by_id() {
        let entries = vec![
            IndexEntry {
                hash: "a".into(),
                id: "1".into(),
                subfiles: 0,
                size: 1,
                raw: None,
            },
            IndexEntry {
                hash: "b".into(),
                id: "2".into(),
                subfiles: 0,
                size: 2,
                raw: None,
            },
        ];
        let updated = replace_or_append(
            entries.clone(),
            IndexEntry {
                hash: "z".into(),
                id: "2".into(),
                subfiles: 0,
                size: 99,
                raw: None,
            },
        );
        assert_eq!(updated.len(), 2);
        assert_eq!(updated.iter().find(|e| e.id == "2").unwrap().hash, "z");

        let appended = replace_or_append(
            entries,
            IndexEntry {
                hash: "c".into(),
                id: "3".into(),
                subfiles: 0,
                size: 3,
                raw: None,
            },
        );
        assert_eq!(appended.len(), 3);
        assert_eq!(appended.last().unwrap().id, "3");
    }

    #[test]
    fn v4_hash_is_sha256_of_body() {
        let body = b"4\nabc:0:x:0:1\n";
        let entries = vec![IndexEntry {
            hash: "abc".into(),
            id: "x".into(),
            subfiles: 0,
            size: 1,
            raw: None,
        }];
        let got = hash_index(Schema::V4, &entries, body).unwrap();
        assert_eq!(got, sha256_hex(body));
    }

    #[test]
    fn v3_hash_is_sha256_of_concatenated_decoded_child_hashes() {
        // Two children sorted by id; v3 hash is sha256 of decoded hashes,
        // sorted.
        let entries = vec![
            IndexEntry {
                hash: "aa".repeat(32), // 64 hex chars, valid 32 bytes
                id: "z".into(),
                subfiles: 0,
                size: 1,
                raw: None,
            },
            IndexEntry {
                hash: "bb".repeat(32),
                id: "a".into(),
                subfiles: 0,
                size: 2,
                raw: None,
            },
        ];
        // Expected: sha256(bytes("bb"*32) || bytes("aa"*32)) — sorted by id
        // means "a" first, then "z".
        let mut h = Sha256::new();
        h.update(hex::decode("bb".repeat(32)).unwrap());
        h.update(hex::decode("aa".repeat(32)).unwrap());
        let expected = hex::encode(h.finalize());
        let got = hash_index(Schema::V3, &entries, &[]).unwrap();
        assert_eq!(got, expected);
    }

    #[test]
    fn crc32c_matches_known_vector() {
        // From RFC 3720 Appendix B.4: CRC32C of "123456789" is 0xE3069283.
        let v = crc32c::crc32c(b"123456789");
        assert_eq!(v, 0xE306_9283);
    }

    #[test]
    fn urls_anchor_to_sync_host() {
        assert!(SYNC_HOST.contains("remarkable.com"));
        assert!(root_url().starts_with(SYNC_HOST));
        assert!(files_url("abc").starts_with(SYNC_HOST));
        assert!(files_url("abc").ends_with("/abc"));
    }

    #[test]
    fn parse_rejects_line_with_missing_fields() {
        // A short line used to be skipped silently, which dropped that
        // document from the rewritten root. It must now stop the parse.
        let body = b"4\n0:.:2:300\nabc:0:doc1:1:100\ndef:0:doc2\n";
        assert!(parse_index(body).is_err());
    }

    #[test]
    fn parse_rejects_non_numeric_counts() {
        assert!(parse_index(b"3\nabc:80000000:doc1:one:100\n").is_err());
        assert!(parse_index(b"3\nabc:80000000:doc1:1:big\n").is_err());
    }

    #[test]
    fn root_rewrite_keeps_server_lines_byte_for_byte() {
        // `deadbeef` stands in for an entry type this client has never seen.
        let body = b"3\naa:80000000:doc1:4:100\nbb:deadbeef:doc2:2:200\n";
        let (schema, entries) = parse_index(body).unwrap();
        let added = replace_or_append(
            entries,
            IndexEntry {
                hash: "cc".into(),
                id: "doc3".into(),
                subfiles: 1,
                size: 5,
                raw: None,
            },
        );
        let out = String::from_utf8(serialize_index(schema, ".", &added, true)).unwrap();
        assert!(out.contains("aa:80000000:doc1:4:100\n"));
        assert!(out.contains("bb:deadbeef:doc2:2:200\n"));
        assert!(out.contains("cc:80000000:doc3:1:5\n"));
        assert_eq!(out.lines().count(), 4);
    }

    fn root_state_from(body: &str) -> RootState {
        let (schema, entries) = parse_index(body.as_bytes()).unwrap();
        RootState {
            schema,
            root_hash: "old".into(),
            generation: 7,
            entries,
            body: body.to_string(),
        }
    }

    fn one_blob_doc() -> Vec<IndexEntry> {
        vec![IndexEntry {
            hash: "ab".repeat(32),
            id: "doc9.metadata".into(),
            subfiles: 0,
            size: 10,
            raw: None,
        }]
    }

    #[test]
    fn dry_run_diff_shows_one_added_line_and_nothing_removed() {
        let root = root_state_from("4\n0:.:2:300\naa:0:doc1:4:100\nbb:0:doc2:2:200\n");
        let planned = plan_root(&root, "doc9", &one_blob_doc()).unwrap();
        let new_body = String::from_utf8(planned.root_body).unwrap();
        let diff = diff_root(root.schema, &root.body, &new_body);
        assert!(diff.removed.is_empty());
        assert_eq!(diff.added.len(), 1);
        assert!(diff.added[0].contains(":doc9:"));
        assert!(diff.order_preserved);
        assert_eq!(diff.old_entries, 2);
        assert_eq!(diff.new_entries, 3);
        assert_eq!(diff.old_totals.as_deref(), Some("0:.:2:300"));
        assert_eq!(diff.new_totals.as_deref(), Some("0:.:3:310"));
    }

    #[test]
    fn dry_run_diff_flags_a_reordered_index() {
        // The server lists zzz before aaa. serialize_index sorts by id, so
        // the rewrite would reorder existing lines. Nothing is lost, but
        // the dry run has to say so.
        let root = root_state_from("3\nbb:80000000:zzz:2:200\naa:80000000:aaa:4:100\n");
        let planned = plan_root(&root, "mmm", &one_blob_doc()).unwrap();
        let new_body = String::from_utf8(planned.root_body).unwrap();
        let diff = diff_root(root.schema, &root.body, &new_body);
        assert!(diff.removed.is_empty());
        assert_eq!(diff.added.len(), 1);
        assert!(!diff.order_preserved);
    }

    #[test]
    fn dry_run_diff_reports_a_dropped_line() {
        let old = "4\n0:.:2:300\naa:0:doc1:4:100\nbb:0:doc2:2:200\n";
        let new = "4\n0:.:2:210\naa:0:doc1:4:100\ncc:0:doc9:1:10\n";
        let diff = diff_root(Schema::V4, old, new);
        assert_eq!(diff.removed, vec!["bb:0:doc2:2:200".to_string()]);
        assert_eq!(diff.added, vec!["cc:0:doc9:1:10".to_string()]);
        assert!(!diff.order_preserved);
    }

    fn doc(id: &str, name: &str, doc_type: &str) -> DocumentInfo {
        DocumentInfo {
            id: id.into(),
            visible_name: name.into(),
            doc_type: doc_type.into(),
            parent: None,
            deleted: false,
        }
    }

    fn skip(id: &str) -> SkippedEntry {
        SkippedEntry {
            id: id.into(),
            error: "timeout".into(),
        }
    }

    #[test]
    fn partition_sorts_docs_and_keeps_every_skip() {
        let results = vec![
            Ok(doc("d2", "zeta", "DocumentType")),
            Err(skip("s2")),
            Ok(doc("f1", "Folder", "CollectionType")),
            Err(skip("s1")),
            Ok(doc("d1", "alpha", "DocumentType")),
        ];
        let listing = partition_listing(results);
        let ids: Vec<&str> = listing.docs.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["f1", "d1", "d2"]);
        let skipped: Vec<&str> = listing.skipped.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(skipped, ["s1", "s2"]);
    }

    #[test]
    fn incomplete_summary_names_every_skipped_id() {
        let results = vec![
            Ok(doc("d1", "alpha", "DocumentType")),
            Err(skip("s1")),
            Err(skip("s2")),
        ];
        let listing = partition_listing(results);
        let summary = listing.incomplete_summary().expect("must be flagged");
        assert!(summary.starts_with("INCOMPLETE: 2 of 3 root entries"));
        assert!(summary.contains("s1"));
        assert!(summary.contains("s2"));
    }

    #[test]
    fn incomplete_summary_is_none_when_everything_was_read() {
        let results = vec![Ok(doc("d1", "alpha", "DocumentType"))];
        let listing = partition_listing(results);
        assert!(listing.incomplete_summary().is_none());
    }
}
#[test]
fn signed_upload_url_requires_approved_https_host() {
    assert!(validate_signed_upload_url(
        "https://storage.googleapis.com/bucket/object?signature=test"
    )
    .is_ok());
    assert!(validate_signed_upload_url("http://storage.googleapis.com/bucket/object").is_err());
    assert!(validate_signed_upload_url("https://127.0.0.1/object").is_err());
    assert!(validate_signed_upload_url("https://example.com/object").is_err());
    assert!(validate_signed_upload_url("https://user@example.amazonaws.com/object").is_err());
}
