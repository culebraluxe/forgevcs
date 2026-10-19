//! W5 `WorkspaceService`: Scion's real working-copy implementation behind the REST façade.
//!
//! Each workspace is a disposable materialization of a changeset in a `tempfile::TempDir`.
//! The canonical state stays in Neon; losing a workspace loses nothing but the cache. The
//! working copy is operated by `scion-wc` against the engine's private gRPC surface (a Unix
//! socket), so tree building, diffing, uploads, commits and land are Scion's code, not a second
//! implementation. Every mutation still passes the façade's external policy check because the
//! internal client's principal is deliberately privileged.
//!
//! Workspace state and dirty-file overlays are persisted in Neon (`forgevcs_workspaces`,
//! `forgevcs_workspace_files`). The temp directory is only a cache: any container can restore a
//! workspace from its persisted base state and overlay before serving it. Vercel's scale-to-zero
//! and multi-instance routing therefore do not lose a workspace ID.

use std::sync::Arc;
use std::time::SystemTime;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use deadpool_postgres::Pool;
use scion_core::{Mode, Principal, ScionError};
use scion_object::{RefName, RepoPath};
use scion_policy::Policy;
use scion_proto::{ScionClient, status_from_error};
use scion_wc::fs_safe::SafeRoot;
use scion_wc::state::save as save_wc_state;
use scion_wc::{WcOptions, WorkingCopy};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::Mutex;
use tonic::Status;
use uuid::Uuid;

use crate::config::Config;
use crate::lease::{LeaseError, LeaseGrant, PgLeaseStore, WorkspaceLease};

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
    repo: String,
    pool: Pool,
    /// Per-workspace write lease (R4). Every path that persists a workspace change acquires it
    /// first, so two agents writing the same workspace cannot silently overwrite each other.
    lease: WorkspaceLease,
}

const CREATE_WORKSPACES: &str = r#"
CREATE TABLE IF NOT EXISTS forgevcs_workspaces (
    id         UUID PRIMARY KEY,
    repo       TEXT NOT NULL,
    state      JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
)
"#;

const CREATE_FILES: &str = r#"
CREATE TABLE IF NOT EXISTS forgevcs_workspace_files (
    workspace_id UUID NOT NULL REFERENCES forgevcs_workspaces(id) ON DELETE CASCADE,
    path         TEXT NOT NULL,
    mode         INT2 NOT NULL,
    data         BYTEA,
    PRIMARY KEY (workspace_id, path)
)
"#;

/// Create the workspace tables when they do not exist. Shared by the boot path and the R4
/// Postgres lease test, which needs a workspace row to hold a lease on.
pub(crate) async fn ensure_schema(pool: &Pool) -> Result<(), String> {
    let client = pool.get().await.map_err(|error| error.to_string())?;
    client
        .batch_execute(&format!("{CREATE_WORKSPACES}; {CREATE_FILES}"))
        .await
        .map_err(|error| format!("creating workspace tables: {error}"))
}

impl WorkspaceService {
    /// Build a service over the engine's internal gRPC client and the shared Postgres pool.
    pub async fn connect(client: ScionClient, repo: &str, config: &Config) -> Result<Self, String> {
        let url = config
            .database_url
            .as_deref()
            .ok_or_else(|| "workspace persistence needs FORGEVCS_DATABASE_URL".to_owned())?;
        let pool = crate::db::build_pool(url)?;
        ensure_schema(&pool).await?;
        crate::lease::ensure_schema(&pool).await?;
        let lease = WorkspaceLease::new(
            Arc::new(PgLeaseStore::new(pool.clone())),
            config.workspace_lease_ttl(),
        );
        Ok(Self {
            client,
            repo: repo.to_owned(),
            pool,
            lease,
        })
    }

    /// The lease owner for a caller: the authenticated principal's stable id string. The lease is
    /// per principal, not per process, so a restarted agent can re-acquire its own workspace.
    fn owner(who: &Principal) -> String {
        who.id_string()
    }

    fn workspace_uuid(id: &str) -> WsResult<Uuid> {
        Uuid::parse_str(id)
            .map_err(|_| Status::not_found(format!("workspace {id:?} does not exist")))
    }

    /// Acquire the write lease for a caller, mapping a lease conflict onto the typed status the
    /// façade renders as HTTP 409 with the current owner.
    async fn acquire_lease(&self, id: &str, who: &Principal) -> WsResult<LeaseGrant> {
        let workspace_id = Self::workspace_uuid(id)?;
        self.lease
            .acquire(
                workspace_id,
                &self.repo,
                &Self::owner(who),
                SystemTime::now(),
            )
            .await
            .map_err(LeaseError::into_status)
    }

    /// Renew the lease immediately before persisting a change. If the lease was taken over while
    /// this request was working, the change is abandoned instead of overwriting the new owner.
    async fn renew_lease(&self, grant: &LeaseGrant) -> WsResult<LeaseGrant> {
        self.lease
            .heartbeat(grant, SystemTime::now())
            .await
            .map_err(LeaseError::into_status)
    }

    async fn entry(&self, id: &str) -> WsResult<Arc<Entry>> {
        // Deliberately no cross-request cache: another Vercel instance may have changed the
        // persisted overlay since this container last looked. Each request restores from Neon,
        // and the TempDir is dropped when the request ends.
        Ok(Arc::new(self.restore_entry(id).await?))
    }

    async fn restore_entry(&self, id: &str) -> WsResult<Entry> {
        Uuid::parse_str(id)
            .map_err(|_| Status::not_found(format!("workspace {id:?} does not exist")))?;
        let client = self.pool.get().await.map_err(internal)?;
        let row = client
            .query_opt(
                "SELECT state::text AS state FROM forgevcs_workspaces WHERE id = $1::text::uuid AND repo = $2",
                &[&id, &self.repo],
            )
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("workspace {id:?} does not exist")))?;
        let state_text: String = row.get("state");
        let state: scion_wc::state::WcState =
            serde_json::from_str(&state_text).map_err(internal)?;

        let dir = tempfile::tempdir().map_err(internal)?;
        WorkingCopy::checkout(
            self.client.clone(),
            dir.path(),
            &state.base.to_text(),
            state.sparse.clone(),
            WcOptions::default(),
        )
        .await
        .map_err(scion)?;
        save_wc_state(&dir.path().join(".scion"), &state).map_err(scion)?;

        let files = client
            .query(
                "SELECT path, mode, data FROM forgevcs_workspace_files WHERE workspace_id = $1::text::uuid",
                &[&id],
            )
            .await
            .map_err(internal)?;
        if !files.is_empty() {
            let root = SafeRoot::open(dir.path()).map_err(scion)?;
            for file in files {
                let path: String = file.get("path");
                let path = RepoPath::parse(path.as_bytes()).map_err(scion)?;
                let mode = Mode::from_u8(file.get::<_, i16>("mode") as u8).map_err(scion)?;
                match file.get::<_, Option<Vec<u8>>>("data") {
                    Some(data) => match mode {
                        Mode::Symlink => root.symlink(&path, &data).map_err(scion)?,
                        Mode::File | Mode::Executable => {
                            root.write_file_atomic(&path, &data, mode).map_err(scion)?;
                        }
                        Mode::Tree => return Err(internal("persisted tree overlay")),
                    },
                    None => root.remove(&path).map_err(scion)?,
                }
            }
        }
        Ok(Entry {
            dir,
            lock: Mutex::new(()),
        })
    }

    async fn persist_state(&self, id: &str, state: &scion_wc::state::WcState) -> WsResult<()> {
        let json = serde_json::to_string(state).map_err(internal)?;
        let client = self.pool.get().await.map_err(internal)?;
        client
            .execute(
                "UPDATE forgevcs_workspaces SET state = $1::text::jsonb WHERE id = $2::text::uuid AND repo = $3",
                &[&json, &id, &self.repo],
            )
            .await
            .map_err(internal)?;
        Ok(())
    }

    async fn persist_file(
        &self,
        id: &str,
        path: &RepoPath,
        mode: Mode,
        data: &[u8],
        deleted: bool,
    ) -> WsResult<()> {
        let path = path_text(path);
        let client = self.pool.get().await.map_err(internal)?;
        if deleted {
            client
                .execute(
                    "DELETE FROM forgevcs_workspace_files WHERE workspace_id = $1::text::uuid AND path = $2",
                    &[&id, &path],
                )
                .await
                .map_err(internal)?;
        } else {
            client
                .execute(
                    "INSERT INTO forgevcs_workspace_files (workspace_id, path, mode, data)
                     VALUES ($1::text::uuid, $2, $3, $4)
                     ON CONFLICT (workspace_id, path) DO UPDATE SET mode = $3, data = $4",
                    &[&id, &path, &(mode as u8 as i16), &data],
                )
                .await
                .map_err(internal)?;
        }
        Ok(())
    }

    async fn clear_files(&self, id: &str) -> WsResult<()> {
        let client = self.pool.get().await.map_err(internal)?;
        client
            .execute(
                "DELETE FROM forgevcs_workspace_files WHERE workspace_id = $1::text::uuid",
                &[&id],
            )
            .await
            .map_err(internal)?;
        Ok(())
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
        let state_text = serde_json::to_string(wc.state()).map_err(internal)?;
        let state = state_json(wc.state());
        let id = Uuid::new_v4().to_string();
        let client = self.pool.get().await.map_err(internal)?;
        client
            .execute(
                "INSERT INTO forgevcs_workspaces (id, repo, state) VALUES ($1::text::uuid, $2, $3::text::jsonb)",
                &[&id, &self.repo, &state_text],
            )
            .await
            .map_err(internal)?;
        drop(dir);
        Ok((id, state))
    }

    /// Move an existing workspace to another ref/changeset. Holds the workspace write lease
    /// while the goto rewrites the working copy and persists the new state.
    pub async fn materialize(
        &self,
        id: &str,
        rev: &str,
        force: bool,
        who: &Principal,
    ) -> WsResult<Value> {
        let grant = self.acquire_lease(id, who).await?;
        let entry = self.entry(id).await?;
        let _guard = entry.lock.lock().await;
        let mut wc = self.open(&entry)?;
        let report = wc.goto(rev, force).await.map_err(scion)?;
        // Renew before persisting: if another owner took the lease over while the goto ran, the
        // rebuilt working copy must not overwrite theirs.
        self.renew_lease(&grant).await?;
        self.persist_state(id, wc.state()).await?;
        if report.preserved_dirty.is_empty() {
            self.clear_files(id).await?;
        }
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
        // Acquire before restoring the working copy, so this request starts from the overlay the
        // previous owner left (and cannot race a live writer).
        let grant = self.acquire_lease(id, who).await?;
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
        // Renew immediately before the persisted overlay changes; losing the lease aborts the
        // write instead of last-writer-winning.
        self.renew_lease(&grant).await?;
        self.persist_file(id, &path, mode, data, false).await?;
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
        let grant = self.acquire_lease(id, who).await?;
        let entry = self.entry(id).await?;
        let _guard = entry.lock.lock().await;
        let root = SafeRoot::open(entry.dir.path()).map_err(scion)?;
        root.remove(&path).map_err(scion)?;
        self.renew_lease(&grant).await?;
        self.persist_file(id, &path, Mode::File, &[], true).await?;
        Ok(json!({
            "workspace_id": id,
            "path": path_text(&path),
            "deleted": true,
        }))
    }

    /// Commit the workspace, then land it on `ref_name` (or leave it unlanded with `mode=none`).
    ///
    /// The lease is acquired before the working copy is restored and released on every exit
    /// path, so the next writer continues immediately instead of waiting for expiry.
    pub async fn submit(
        &self,
        id: &str,
        request: SubmitRequest<'_>,
        policy: &Policy,
        who: &Principal,
    ) -> WsResult<Value> {
        if request.mode != "land" && request.mode != "none" {
            return Err(Status::invalid_argument("mode must be land or none"));
        }
        let grant = self.acquire_lease(id, who).await?;
        let result = self.submit_holding(id, request, policy, who, &grant).await;
        if let Err(error) = self.lease.release(&grant, SystemTime::now()).await {
            // The commit/land outcome is already decided; a failed release only means the lease
            // will be recovered by expiry (or the next writer's audited takeover).
            tracing::warn!(workspace_id = id, %error, "releasing workspace lease failed");
        }
        result
    }

    async fn submit_holding(
        &self,
        id: &str,
        request: SubmitRequest<'_>,
        policy: &Policy,
        who: &Principal,
        grant: &LeaseGrant,
    ) -> WsResult<Value> {
        let SubmitRequest {
            message,
            author,
            ref_name,
            mode,
        } = request;
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
        self.renew_lease(grant).await?;
        let commit = wc
            .commit_with_report(message, author, None)
            .await
            .map_err(scion)?;
        let report = report_json(&commit);
        let mut ref_value = Value::Null;
        if mode == "land" {
            // The ref move is the critical write: verify the lease is still ours, then land.
            self.renew_lease(grant).await?;
            let landed = wc.land(&parsed_ref).await.map_err(scion)?;
            ref_value = land_json(&landed);
        }
        let final_state = wc.state().clone();
        self.clear_files(id).await?;
        self.persist_state(id, &final_state).await?;
        Ok(json!({
            "workspace_id": id,
            "changeset": report,
            "ref": ref_value,
            "state": state_json(&final_state),
        }))
    }

    /// Destroy a workspace and its temporary directory. The caller must hold the write lease; the
    /// release is audited after the workspace row (and its lease row) are gone.
    pub async fn destroy(&self, id: &str, who: &Principal) -> WsResult<()> {
        let grant = self.acquire_lease(id, who).await?;
        let client = self.pool.get().await.map_err(internal)?;
        let deleted = client
            .execute(
                "DELETE FROM forgevcs_workspaces WHERE id = $1::text::uuid AND repo = $2",
                &[&id, &self.repo],
            )
            .await
            .map_err(internal)?;
        if deleted == 0 {
            return Err(Status::not_found(format!(
                "workspace {id:?} does not exist"
            )));
        }
        if let Err(error) = self.lease.record_released(&grant, SystemTime::now()).await {
            // The workspace is already gone; a missing audit row is a warning, not a failed
            // destroy (the row it describes no longer exists to protect).
            tracing::warn!(workspace_id = id, %error, "recording workspace lease release failed");
        }
        Ok(())
    }
}
