//! W5 `WorkspaceService`: Scion's real working-copy implementation behind the REST façade.
//!
//! Each workspace is a disposable materialization of a changeset in a `tempfile::TempDir`.
//! The canonical state stays in Neon; losing a workspace loses nothing but the cache. The
//! working copy is operated by `scion-wc` against the engine's private gRPC surface (a Unix
//! socket), so tree building, diffing, uploads, commits and land are Scion's code, not a second
//! implementation. Every mutation still passes the façade's external policy check because the
//! internal client's principal is deliberately privileged.
//!
//! Workspaces live in the process (and therefore in the container's warm lifetime). A restart
//! empties the registry; the IDs answer 404 and a client creates a new workspace from a ref or
//! changeset. That is the V0 semantics: a workspace is cache, never authoritative.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use scion_core::{Mode, Principal, ScionError};
use scion_object::{RefName, RepoPath};
use scion_policy::Policy;
use scion_proto::{ScionClient, status_from_error};
use scion_wc::fs_safe::SafeRoot;
use scion_wc::{WcOptions, WorkingCopy};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::Mutex;
use tonic::Status;
use uuid::Uuid;

/// Default author of a workspace commit.
pub const DEFAULT_AUTHOR: &[u8] = b"ForgeVCS workspace <workspace@example.test>";

/// The commit/submit request of a workspace.
pub struct SubmitRequest<'a> {
    /// Commit message.
    pub message: &'a [u8],
    /// Commit author.
    pub author: &'a [u8],
    /// Ref to land, when `mode` is `land`.
    pub ref_name: &'a str,
    /// `land` or `none`.
    pub mode: &'a str,
}

type WsResult<T> = Result<T, Status>;

fn scion(error: ScionError) -> Status {
    status_from_error(&error)
}

fn internal(error: impl std::fmt::Display) -> Status {
    Status::internal(error.to_string())
}

fn path_text(path: &RepoPath) -> String {
    String::from_utf8_lossy(path.as_bytes()).into_owned()
}

fn parse_path(raw: &str) -> WsResult<RepoPath> {
    RepoPath::parse(raw.as_bytes()).map_err(scion)
}

fn state_json(state: &scion_wc::state::WcState) -> Value {
    json!({
        "base": state.base.to_text(),
        "tracking_ref": state.tracking_ref.as_ref().map(RefName::as_str),
        "sparse": state.sparse.iter().map(|path| {
            if path.is_root() { "/".to_owned() } else { path_text(path) }
        }).collect::<Vec<_>>(),
    })
}

fn report_json(report: &scion_wc::CommitReport) -> Value {
    json!({
        "changeset": report.changeset.to_text(),
        "root_tree": report.root_tree.to_text(),
        "change_id": hex::encode(report.change_id.0),
        "parents": report.parents.iter().map(|id| id.to_text()).collect::<Vec<_>>(),
        "delta": report.delta.iter().map(|entry| path_text(&entry.path)).collect::<Vec<_>>(),
        "uploaded": report.uploaded,
    })
}

fn land_json(report: &scion_wc::LandReport) -> Value {
    json!({
        "ref": report.ref_name,
        "target": report.target,
        "version": report.version,
        "landed": report.landed,
        "head": report.head,
        "moved": report.moved,
    })
}

struct Entry {
    dir: TempDir,
    /// Serializes operations on one workspace. A `scion-wc` command takes the on-disk lock
    /// itself, but two REST calls can still interleave between `open` and that command.
    lock: Mutex<()>,
}

/// The process-wide workspace registry of one engine.
pub struct WorkspaceService {
    client: ScionClient,
    entries: Mutex<HashMap<String, Arc<Entry>>>,
}

impl WorkspaceService {
    /// Build a service over the engine's internal gRPC client.
    pub fn new(client: ScionClient) -> Self {
        Self {
            client,
            entries: Mutex::new(HashMap::new()),
        }
    }

    async fn entry(&self, id: &str) -> WsResult<Arc<Entry>> {
        self.entries.lock().await.get(id).cloned().ok_or_else(|| {
            Status::not_found(format!("workspace {id:?} does not exist in this process"))
        })
    }

    fn open(&self, entry: &Entry) -> WsResult<WorkingCopy> {
        WorkingCopy::open(entry.dir.path(), self.client.clone(), WcOptions::default())
            .map_err(scion)
    }

    /// Materialize `rev` (a ref name or changeset id) into a new disposable workspace.
    pub async fn create(&self, rev: &str, sparse: Vec<RepoPath>) -> WsResult<(String, Value)> {
        let dir = tempfile::tempdir().map_err(internal)?;
        let wc = WorkingCopy::checkout(
            self.client.clone(),
            dir.path(),
            rev,
            sparse,
            WcOptions::default(),
        )
        .await
        .map_err(scion)?;
        let state = state_json(wc.state());
        let id = Uuid::new_v4().to_string();
        self.entries.lock().await.insert(
            id.clone(),
            Arc::new(Entry {
                dir,
                lock: Mutex::new(()),
            }),
        );
        Ok((id, state))
    }

    /// Move an existing workspace to another ref/changeset.
    pub async fn materialize(&self, id: &str, rev: &str, force: bool) -> WsResult<Value> {
        let entry = self.entry(id).await?;
        let _guard = entry.lock.lock().await;
        let mut wc = self.open(&entry)?;
        let report = wc.goto(rev, force).await.map_err(scion)?;
        Ok(json!({
            "workspace_id": id,
            "state": state_json(wc.state()),
            "goto": {
                "from": report.from,
                "to": report.to,
                "written": report.written,
                "removed": report.removed,
                "preserved_dirty": report.preserved_dirty,
            },
        }))
    }

    /// Working-copy status, filtered to paths the external caller may read.
    pub async fn status(&self, id: &str, policy: &Policy, who: &Principal) -> WsResult<Value> {
        let entry = self.entry(id).await?;
        let _guard = entry.lock.lock().await;
        let mut wc = self.open(&entry)?;
        let report = wc.status().await.map_err(scion)?;
        let paths = |paths: &[RepoPath]| -> Vec<String> {
            paths
                .iter()
                .filter(|path| policy.can_read(who, path))
                .map(path_text)
                .collect()
        };
        Ok(json!({
            "workspace_id": id,
            "state": state_json(wc.state()),
            "modified": paths(&report.modified),
            "added": paths(&report.added),
            "deleted": paths(&report.deleted),
            "mode_changed": paths(&report.mode_changed),
            "redacted_dirty": paths(&report.redacted_dirty),
            "hashed": report.hashed,
            "clean": report.is_clean(),
        }))
    }

    /// Working-copy diff with hunks, filtered to paths the external caller may read.
    pub async fn diff(
        &self,
        id: &str,
        context: usize,
        policy: &Policy,
        who: &Principal,
    ) -> WsResult<Value> {
        let entry = self.entry(id).await?;
        let _guard = entry.lock.lock().await;
        let mut wc = self.open(&entry)?;
        let changes = wc.diff_with_hunks(context).await.map_err(scion)?;
        let mut visible = Vec::new();
        for change in changes {
            let Ok(path) = RepoPath::parse(change.path.as_bytes()) else {
                continue;
            };
            if policy.can_read(who, &path) {
                visible.push(serde_json::to_value(change).map_err(internal)?);
            }
        }
        Ok(json!({
            "workspace_id": id,
            "state": state_json(wc.state()),
            "changes": visible,
        }))
    }

    /// Read one file.
    pub async fn read_file(
        &self,
        id: &str,
        path: &str,
        policy: &Policy,
        who: &Principal,
    ) -> WsResult<Value> {
        let path = parse_path(path)?;
        if !policy.can_read(who, &path) {
            return Err(Status::permission_denied("read on the path denied"));
        }
        let entry = self.entry(id).await?;
        let _guard = entry.lock.lock().await;
        let root = SafeRoot::open(entry.dir.path()).map_err(scion)?;
        let data = root.read(&path).map_err(scion)?;
        let stat = root
            .stat(&path)
            .map_err(scion)?
            .ok_or_else(|| Status::not_found(format!("{} does not exist", path_text(&path))))?;
        Ok(json!({
            "workspace_id": id,
            "path": path_text(&path),
            "size": data.len(),
            "unix_mode": stat.mode,
            "data": BASE64.encode(&data),
        }))
    }

    /// Write one file (or a symlink's target with mode 3).
    pub async fn write_file(
        &self,
        id: &str,
        path: &str,
        data: &[u8],
        mode: u8,
        policy: &Policy,
        who: &Principal,
    ) -> WsResult<Value> {
        let path = parse_path(path)?;
        if path.is_root() {
            return Err(Status::invalid_argument("cannot write the repository root"));
        }
        if !policy.can_write(who, &path) {
            return Err(Status::permission_denied("write on the path denied"));
        }
        let mode = Mode::from_u8(mode).map_err(scion)?;
        if mode == Mode::Tree {
            return Err(Status::invalid_argument("cannot write a tree entry"));
        }
        let entry = self.entry(id).await?;
        let _guard = entry.lock.lock().await;
        let root = SafeRoot::open(entry.dir.path()).map_err(scion)?;
        match mode {
            Mode::Symlink => root.symlink(&path, data).map_err(scion)?,
            Mode::File | Mode::Executable => {
                root.write_file_atomic(&path, data, mode).map_err(scion)?;
            }
            Mode::Tree => {
                return Err(Status::invalid_argument("cannot write a tree entry"));
            }
        }
        Ok(json!({
            "workspace_id": id,
            "path": path_text(&path),
            "size": data.len(),
            "mode": mode as u8,
        }))
    }

    /// Delete one file.
    pub async fn delete_file(
        &self,
        id: &str,
        path: &str,
        policy: &Policy,
        who: &Principal,
    ) -> WsResult<Value> {
        let path = parse_path(path)?;
        if path.is_root() {
            return Err(Status::invalid_argument(
                "cannot delete the repository root",
            ));
        }
        if !policy.can_write(who, &path) {
            return Err(Status::permission_denied("write on the path denied"));
        }
        let entry = self.entry(id).await?;
        let _guard = entry.lock.lock().await;
        let root = SafeRoot::open(entry.dir.path()).map_err(scion)?;
        root.remove(&path).map_err(scion)?;
        Ok(json!({
            "workspace_id": id,
            "path": path_text(&path),
            "deleted": true,
        }))
    }

    /// Commit the workspace, then land it on `ref_name` (or leave it unlanded with `mode=none`).
    pub async fn submit(
        &self,
        id: &str,
        request: SubmitRequest<'_>,
        policy: &Policy,
        who: &Principal,
    ) -> WsResult<Value> {
        let SubmitRequest {
            message,
            author,
            ref_name,
            mode,
        } = request;
        if mode != "land" && mode != "none" {
            return Err(Status::invalid_argument("mode must be land or none"));
        }
        let entry = self.entry(id).await?;
        let _guard = entry.lock.lock().await;
        let mut wc = self.open(&entry)?;
        let status = wc.status().await.map_err(scion)?;
        for path in status
            .modified
            .iter()
            .chain(&status.added)
            .chain(&status.deleted)
            .chain(&status.mode_changed)
        {
            if !policy.can_write(who, path) {
                return Err(Status::permission_denied(format!(
                    "write on {} denied",
                    path_text(path)
                )));
            }
        }
        let parsed_ref = RefName::parse(ref_name).map_err(scion)?;
        if mode == "land" && !policy.can_update_ref(who, &parsed_ref) {
            return Err(Status::permission_denied(format!(
                "update on ref {ref_name} denied"
            )));
        }
        let commit = wc
            .commit_with_report(message, author, None)
            .await
            .map_err(scion)?;
        let report = report_json(&commit);
        let mut ref_value = Value::Null;
        if mode == "land" {
            let landed = wc.land(&parsed_ref).await.map_err(scion)?;
            ref_value = land_json(&landed);
        }
        Ok(json!({
            "workspace_id": id,
            "changeset": report,
            "ref": ref_value,
            "state": state_json(wc.state()),
        }))
    }

    /// Destroy a workspace and its temporary directory.
    pub async fn destroy(&self, id: &str) -> WsResult<()> {
        let removed = self.entries.lock().await.remove(id);
        match removed {
            Some(entry) => {
                // Drop under the map lock is fine: TempDir removal is synchronous.
                drop(entry);
                Ok(())
            }
            None => Err(Status::not_found(format!(
                "workspace {id:?} does not exist in this process"
            ))),
        }
    }
}
