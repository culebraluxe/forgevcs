//! Pure request mappers: JSON bodies and query strings → the `scion.v1` messages the vendored
//! handlers consume.
//!
//! Nothing here evaluates policy, touches an engine or builds a response. A handler in `super`
//! orchestrates; this module only owns the DTO shapes, their validation and their proto
//! conversion, so the wire contract can be read without the handler bodies.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use prost::Message as _;
use scion_core::ObjectId;
use scion_proto::{convert, v1};
use serde::Deserialize;
use serde_json::Value;

use super::{DEFAULT_LIMIT, DEFAULT_REF};

/// Delta op for a Put (`1`).
pub(crate) const DELTA_PUT: u32 = 1;

// ---------------------------------------------------------------------------------------------
// Id parsing
// ---------------------------------------------------------------------------------------------

pub(crate) fn parse_id(raw: &str, field: &str) -> Result<ObjectId, String> {
    ObjectId::parse_text(raw).map_err(|error| format!("{field}: {}", error.message))
}

pub(crate) fn parse_change_id(raw: &str) -> Result<Vec<u8>, String> {
    let bytes = hex::decode(raw).map_err(|error| format!("change_id is not hex: {error}"))?;
    if bytes.len() != 16 {
        return Err(format!(
            "change_id must be 16 bytes (32 hex digits), got {} bytes",
            bytes.len()
        ));
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------------------------
// Defaults shared by the query/body shapes
// ---------------------------------------------------------------------------------------------

fn default_ref() -> String {
    DEFAULT_REF.to_owned()
}

fn default_limit() -> u32 {
    DEFAULT_LIMIT
}

fn default_true() -> bool {
    true
}

fn default_push_mode() -> String {
    "land".to_owned()
}

fn default_write_mode() -> u8 {
    1
}

fn default_context() -> usize {
    3
}

fn put_op() -> u32 {
    DELTA_PUT
}

fn file_mode() -> u32 {
    1
}

// ---------------------------------------------------------------------------------------------
// Read queries
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct CommitsQuery {
    #[serde(rename = "ref", default = "default_ref")]
    pub(crate) ref_name: String,
    #[serde(default = "default_limit")]
    pub(crate) limit: u32,
    pub(crate) path: Option<String>,
    #[serde(default)]
    pub(crate) first_parent_only: bool,
    /// Opaque continuation from a previous `next_page_token`, hex-encoded.
    pub(crate) page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FileQuery {
    pub(crate) changeset: String,
    #[serde(default)]
    pub(crate) path: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TreeQuery {
    pub(crate) changeset: String,
    #[serde(default)]
    pub(crate) path: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DiffQuery {
    pub(crate) a: Option<String>,
    pub(crate) b: String,
    /// Comma-separated paths; empty/absent means the whole tree.
    pub(crate) paths: Option<String>,
    #[serde(default = "default_true")]
    pub(crate) include_hunks: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MergeBaseQuery {
    pub(crate) a: String,
    pub(crate) b: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PullBody {
    #[serde(rename = "ref", default = "default_ref")]
    pub(crate) ref_name: String,
    #[serde(default = "default_limit")]
    pub(crate) limit: u32,
    pub(crate) path: Option<String>,
    #[serde(default)]
    pub(crate) first_parent_only: bool,
    /// Opaque continuation from a previous `next_page_token`, hex-encoded.
    pub(crate) page_token: Option<String>,
    /// Object ids whose bytes should accompany the history. Empty means metadata only.
    #[serde(default)]
    pub(crate) objects: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Write bodies
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct ObjectUpload {
    pub(crate) id: String,
    /// `0x01` blob or `0x02` tree. Defaults to the kind encoded in the id.
    pub(crate) kind: Option<u32>,
    /// Base64 of the raw bytes (a blob's content or a tree's canonical encoding).
    pub(crate) data: String,
}

impl ObjectUpload {
    pub(crate) fn into_proto(self) -> Result<v1::UploadItem, String> {
        let id = parse_id(&self.id, "objects[].id")?;
        let kind = self.kind.unwrap_or(u32::from(id.kind().byte()));
        let data = BASE64
            .decode(self.data.as_bytes())
            .map_err(|error| format!("objects[{}]: data is not base64: {error}", self.id))?;
        Ok(v1::UploadItem {
            id: id.as_bytes().to_vec(),
            kind,
            data,
        })
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct TimeBody {
    #[serde(default)]
    pub(crate) seconds: i64,
    #[serde(default)]
    pub(crate) tz_offset_minutes: i64,
}

impl TimeBody {
    pub(crate) fn into_proto(self) -> v1::Timestamp {
        v1::Timestamp {
            seconds: self.seconds,
            tz_offset_minutes: self.tz_offset_minutes,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ExtensionBody {
    pub(crate) tag: u64,
    /// The extension value as UTF-8 bytes. Binary extensions are not part of the v1 façade.
    pub(crate) value: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MetaBody {
    /// `author <email>` in UTF-8, matching upstream's bytes field.
    pub(crate) author: String,
    pub(crate) author_time: Option<TimeBody>,
    pub(crate) committer: Option<String>,
    pub(crate) committer_time: Option<TimeBody>,
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) extensions: Vec<ExtensionBody>,
}

impl MetaBody {
    pub(crate) fn into_proto(self) -> v1::MetaFields {
        v1::MetaFields {
            author: self.author.into_bytes(),
            author_time: Some(self.author_time.unwrap_or_default().into_proto()),
            committer: self.committer.map(String::into_bytes),
            committer_time: self.committer_time.map(TimeBody::into_proto),
            message: self.message.into_bytes(),
            extensions: self
                .extensions
                .into_iter()
                .map(|extension| v1::Extension {
                    tag: extension.tag,
                    value: extension.value.into_bytes(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct DeltaBody {
    /// Repository path, UTF-8 (SPEC 4.3.4; paths are sorted ascending by the caller).
    pub(crate) path: String,
    /// `1` Put, `2` Delete.
    #[serde(default = "put_op")]
    pub(crate) op: u32,
    /// `1` File, `2` Executable, `3` Symlink for a Put.
    #[serde(default = "file_mode")]
    pub(crate) mode: u32,
    /// Blob id for a Put; empty for a Delete.
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) size: u64,
}

impl DeltaBody {
    pub(crate) fn into_proto(self) -> Result<v1::DeltaEntryMsg, String> {
        let id = if self.id.is_empty() {
            Vec::new()
        } else {
            parse_id(&self.id, "changeset.delta[].id")?
                .as_bytes()
                .to_vec()
        };
        if self.op == DELTA_PUT && id.is_empty() {
            return Err("changeset.delta[].id is required for a put".to_owned());
        }
        Ok(v1::DeltaEntryMsg {
            path: self.path.into_bytes(),
            op: self.op,
            mode: self.mode,
            id,
            size: self.size,
        })
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChangesetBody {
    #[serde(default)]
    pub(crate) parents: Vec<String>,
    #[serde(default)]
    pub(crate) delta: Vec<DeltaBody>,
    pub(crate) meta: MetaBody,
    /// 16 bytes as 32 hex digits.
    pub(crate) change_id: String,
    pub(crate) expected_root_tree: Option<String>,
    #[serde(default)]
    pub(crate) import_mode: bool,
}

impl ChangesetBody {
    pub(crate) fn into_proto(self) -> Result<v1::CreateChangesetRequest, String> {
        let parents = self
            .parents
            .iter()
            .map(|raw| parse_id(raw, "changeset.parents[]"))
            .collect::<Result<Vec<_>, _>>()?;
        let delta = self
            .delta
            .into_iter()
            .map(DeltaBody::into_proto)
            .collect::<Result<Vec<_>, _>>()?;
        let expected_root_tree = self
            .expected_root_tree
            .as_deref()
            .map(|raw| parse_id(raw, "changeset.expected_root_tree"))
            .transpose()?
            .map(|id| id.as_bytes().to_vec());
        Ok(v1::CreateChangesetRequest {
            parents: parents.iter().map(convert::id_bytes).collect(),
            delta,
            meta_fields: Some(self.meta.into_proto()),
            change_id: parse_change_id(&self.change_id)?,
            expected_root_tree,
            import_mode: self.import_mode,
        })
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct PushBody {
    #[serde(default)]
    pub(crate) objects: Vec<ObjectUpload>,
    pub(crate) changeset: ChangesetBody,
    #[serde(rename = "ref", default = "default_ref")]
    pub(crate) ref_name: String,
    /// `land` (default), `update` or `none`.
    #[serde(default = "default_push_mode")]
    pub(crate) mode: String,
    /// For `update`: the ref version the caller observed.
    #[serde(default)]
    pub(crate) expected_version: u64,
    #[serde(default)]
    pub(crate) allow_non_fast_forward: bool,
    /// For `land`: tolerate an EMPTY_CHANGE rebase result.
    #[serde(default)]
    pub(crate) allow_empty: bool,
}

/// `POST /repos/{repo}/refs` — create a ref at an existing changeset.
#[derive(Debug, Deserialize)]
pub(crate) struct RefCreateBody {
    pub(crate) name: String,
    pub(crate) target: String,
    #[serde(default)]
    pub(crate) reason: String,
}

/// `POST /repos/{repo}/rebase` — rebase one changeset onto another.
#[derive(Debug, Deserialize)]
pub(crate) struct RebaseBody {
    pub(crate) changeset: String,
    pub(crate) onto: String,
}

/// `POST /repos/{repo}/merge` — produce a merge changeset from two changesets.
#[derive(Debug, Deserialize)]
pub(crate) struct MergeBody {
    pub(crate) a: String,
    pub(crate) b: String,
    pub(crate) meta: MetaBody,
    pub(crate) change_id: String,
}

/// `POST /repos/{repo}/land` — publish an existing changeset on a ref.
#[derive(Debug, Deserialize)]
pub(crate) struct LandBody {
    #[serde(rename = "ref", default = "default_ref")]
    pub(crate) ref_name: String,
    pub(crate) head: String,
    #[serde(default)]
    pub(crate) allow_empty: bool,
}

/// `POST /repos/{repo}/merge-land` — merge a head into the ref tip and land the result.
#[derive(Debug, Deserialize)]
pub(crate) struct MergeLandBody {
    #[serde(rename = "ref", default = "default_ref")]
    pub(crate) ref_name: String,
    pub(crate) head: String,
    #[serde(default)]
    pub(crate) author: Option<String>,
    #[serde(default)]
    pub(crate) message: Option<String>,
    #[serde(default)]
    pub(crate) change_id: Option<String>,
    /// `squash` (default from `FORGEVCS_MERGE_TOPOLOGY`) or `merge-commit` (opt-in
    /// `merge-commit-v1`: publish the clean tip-parented two-parent merge changeset itself).
    #[serde(default)]
    pub(crate) topology: Option<String>,
}

/// `POST /repos/{repo}/conflict-context` — the three readable sides of a refused merge/rebase.
#[derive(Debug, Deserialize)]
pub(crate) struct ConflictContextBody {
    #[serde(default)]
    pub(crate) a: Option<String>,
    #[serde(default)]
    pub(crate) b: Option<String>,
    #[serde(default)]
    pub(crate) changeset: Option<String>,
    #[serde(default)]
    pub(crate) onto: Option<String>,
}

/// `POST /repos/{repo}/conflicts/escalate` — file a stuck merge/rebase for the DevOps seat.
#[derive(Debug, Deserialize)]
pub(crate) struct EscalateBody {
    #[serde(rename = "ref", default = "default_ref")]
    pub(crate) ref_name: String,
    pub(crate) head: String,
    #[serde(default)]
    pub(crate) onto: Option<String>,
    #[serde(default)]
    pub(crate) base: Option<String>,
    #[serde(default)]
    pub(crate) note: Option<String>,
    #[serde(default)]
    pub(crate) report: Option<Value>,
}

/// `GET /repos/{repo}/conflicts?scope=paused&status=…`
#[derive(Debug, Deserialize)]
pub(crate) struct ConflictQueueQuery {
    #[serde(default)]
    pub(crate) status: Option<String>,
    /// `paused` selects the resolve/continue workflow; absent lists the escalation queue.
    #[serde(default)]
    pub(crate) scope: Option<String>,
}

/// `POST /repos/{repo}/conflicts/{id}/resolve`.
///
/// The same route serves two resources: a paused conflict resolves with `changeset` (the
/// changeset that contains the resolution), while an escalation resolves with a `resolution`
/// note. A paused conflict id wins; escalation bodies never carry `changeset`.
#[derive(Debug, Deserialize)]
pub(crate) struct ResolveEscalationBody {
    #[serde(default)]
    pub(crate) resolution: Option<String>,
    #[serde(default)]
    pub(crate) changeset: Option<String>,
}

/// `POST /repos/{repo}/conflicts/{id}/continue` — land a resolved paused conflict.
#[derive(Debug, Deserialize)]
pub(crate) struct ContinueConflictBody {
    /// Required only when the paused operation had no target ref (merge/rebase).
    #[serde(rename = "ref", default)]
    pub(crate) ref_name: Option<String>,
}

/// `DELETE /repos/{repo}` — the body must repeat the repository name.
#[derive(Debug, Deserialize)]
pub(crate) struct DeleteRepoBody {
    pub(crate) confirm: String,
}

/// `DELETE /repos/{repo}/conflicts?status=…`
#[derive(Debug, Deserialize)]
pub(crate) struct ClearConflictsQuery {
    #[serde(default)]
    pub(crate) status: Option<String>,
}

/// `POST /admin/gc/objects` — dry-run unless `apply` is true.
#[derive(Debug, Deserialize)]
pub(crate) struct GcBody {
    #[serde(default)]
    pub(crate) apply: bool,
    /// Explicit legacy prefix to purge instead of the orphan sweep (e.g. `forgevcs-ci/`).
    #[serde(default)]
    pub(crate) prefix: Option<String>,
}

/// One exported object key and its bytes (base64). Keys are store-relative
/// (`r|q/<repo_id>/<kind>/<fan>/<hex>`).
#[derive(Debug, Deserialize, serde::Serialize)]
pub(crate) struct BlobEntry {
    pub(crate) key: String,
    pub(crate) data: String,
}

/// `POST /repos/{repo}/blobs/import`
#[derive(Debug, Deserialize)]
pub(crate) struct BlobImportBody {
    pub(crate) entries: Vec<BlobEntry>,
}

// ---------------------------------------------------------------------------------------------
// Workspace bodies
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceCreateBody {
    #[serde(rename = "ref")]
    pub(crate) ref_name: Option<String>,
    pub(crate) changeset: Option<String>,
    #[serde(default)]
    pub(crate) sparse: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceMaterializeBody {
    #[serde(rename = "ref")]
    pub(crate) ref_name: Option<String>,
    pub(crate) changeset: Option<String>,
    #[serde(default)]
    pub(crate) force: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceFileQuery {
    pub(crate) path: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceWriteBody {
    pub(crate) data: String,
    #[serde(default = "default_write_mode")]
    pub(crate) mode: u8,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceSubmitBody {
    pub(crate) message: String,
    pub(crate) author: Option<String>,
    #[serde(rename = "ref")]
    pub(crate) ref_name: Option<String>,
    #[serde(default = "default_push_mode")]
    pub(crate) mode: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceDiffQuery {
    #[serde(default = "default_context")]
    pub(crate) context: usize,
}

// ---------------------------------------------------------------------------------------------
// Wire framing and derived request shapes
// ---------------------------------------------------------------------------------------------

pub(crate) fn grpc_frames(items: &[v1::UploadItem]) -> Vec<u8> {
    let mut framed = Vec::new();
    for item in items {
        let message = item.encode_to_vec();
        let length = u32::try_from(message.len()).unwrap_or(u32::MAX);
        framed.push(0);
        framed.extend_from_slice(&length.to_be_bytes());
        framed.extend_from_slice(&message);
    }
    framed
}

/// `Streaming<UploadItem>` over the bytes the handler would have received from tonic.
///
/// `UploadObjects` is the one write handler with a streaming request. The façade already holds
/// every item in memory (the REST body is not streamed), so it builds the five-byte gRPC frame
/// header and the prost payloads itself and lets tonic decode them. No upload rule is copied.
pub(crate) fn upload_stream(items: Vec<v1::UploadItem>) -> tonic::Streaming<v1::UploadItem> {
    let body = axum::body::Body::from(grpc_frames(&items));
    tonic::codec::Streaming::new_request(
        tonic_prost::ProstCodec::<v1::UploadItem, v1::UploadItem>::raw_decoder(Default::default()),
        body,
        None,
        None,
    )
}

/// The repository paths one push writes: every changeset delta path. The façade authorizes each
/// one (FVCS-02), and upstream's `create_changeset` checks the same paths again (defense in
/// depth).
pub(crate) fn push_delta_paths(changeset: &ChangesetBody) -> Vec<String> {
    changeset
        .delta
        .iter()
        .map(|delta| delta.path.clone())
        .collect()
}

/// The metadata `merge-land` and `conflict-context` attach to the unexported changesets they
/// derive. Fixed and content-free so re-running the operation is deterministic.
pub(crate) fn derived_meta(author: &str, message: &str) -> v1::MetaFields {
    v1::MetaFields {
        author: author.as_bytes().to_vec(),
        author_time: Some(v1::Timestamp::default()),
        committer: None,
        committer_time: None,
        message: message.as_bytes().to_vec(),
        extensions: Vec::new(),
    }
}
