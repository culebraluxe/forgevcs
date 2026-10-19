//! R4: per-workspace write lease.
//!
//! A workspace is a disposable working copy restored from Neon on every request, which makes
//! two concurrent agents writing the same workspace a silent last-writer-wins race: each request
//! reads the persisted overlay, mutates it, and writes it back. The lease in this module closes
//! that race with a row in Postgres before the overlay is touched.
//!
//! # Semantics
//!
//! The lease is keyed by workspace id and owned by the authenticated principal (`service:agent-a`,
//! `github:culebraluxe`). It carries a fencing token (`lease_id`), an acquisition time, a
//! heartbeat and an expiry (`FORGEVCS_WORKSPACE_LEASE_TTL_SECS`, default 5 minutes).
//!
//! * **Acquire** — a free workspace gets a new lease; the same owner refreshing its own lease is
//!   idempotent (it renews the heartbeat and expiry and keeps the fencing token); a foreign,
//!   unexpired lease is a typed conflict carrying the current owner, never an overwrite.
//! * **Takeover** — a lease whose `expires_at` is at or before `now` is stale (its holder died or
//!   was abandoned). A new owner may take it over, and the (`takeover`) audit row records the
//!   prior owner, prior token and prior expiry. Takeover requires an expired lease; a live lease
//!   cannot be stolen.
//! * **Heartbeat** — every write path renews after acquiring, and the call sites renew again just
//!   before persisting the overlay/landing the ref. If the lease was taken over in the meantime,
//!   the renewal fails and the mutation is not persisted.
//! * **Release** — `submit` and `destroy` release the lease so the next writer can acquire
//!   immediately instead of waiting for expiry.
//!
//! The decision is one transaction: `INSERT … ON CONFLICT DO NOTHING` (serialises the first
//! writer of a workspace), then `SELECT … FOR UPDATE`, then a refresh/takeover/conflict branch.
//! Two concurrent acquirers therefore cannot both win, even across processes: Postgres, not the
//! service, is the arbiter. This is deliberately a lease table rather than `pg_advisory_lock` —
//! an advisory lock is tied to a pooled session and cannot outlive the request that took it,
//! while a lease row with an owner and expiry is exactly the durable record the 409 must report.
//!
//! The direct-DDL bootstrap in [`ensure_schema`] mirrors `migrations/0004_forgevcs_workspace_leases.sql`;
//! keep the two in step (same pattern as `forgevcs_workspaces`).

#[cfg(test)]
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
#[cfg(test)]
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deadpool_postgres::Pool;
use serde_json::{Value, json};
use tonic::metadata::MetadataMap;
use tonic::{Code, Status};
use uuid::Uuid;

/// Default lease lifetime: five minutes. Configurable through
/// `FORGEVCS_WORKSPACE_LEASE_TTL_SECS`.
pub const DEFAULT_TTL_SECS: u64 = 300;

/// `tonic` metadata key carrying the current owner of a conflicting lease.
pub const METADATA_OWNER: &str = "forgevcs-lease-owner";
/// `tonic` metadata key carrying the conflicting lease's fencing token.
pub const METADATA_LEASE_ID: &str = "forgevcs-lease-id";
/// `tonic` metadata key carrying the conflicting workspace id.
pub const METADATA_WORKSPACE_ID: &str = "forgevcs-lease-workspace";
/// `tonic` metadata key carrying the conflicting lease's expiry (Unix seconds).
pub const METADATA_EXPIRES_AT: &str = "forgevcs-lease-expires-at";
/// `tonic` metadata key carrying the repository of the conflicting workspace.
pub const METADATA_REPO: &str = "forgevcs-lease-repo";

const CREATE_LEASES: &str = r#"
CREATE TABLE IF NOT EXISTS forgevcs_workspace_leases (
    workspace_id UUID PRIMARY KEY REFERENCES forgevcs_workspaces(id) ON DELETE CASCADE,
    repo         TEXT NOT NULL,
    owner_id     TEXT NOT NULL,
    lease_id     UUID NOT NULL,
    acquired_at  TIMESTAMPTZ NOT NULL,
    heartbeat_at TIMESTAMPTZ NOT NULL,
    expires_at   TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS forgevcs_workspace_leases_expiry
    ON forgevcs_workspace_leases (repo, expires_at);
CREATE TABLE IF NOT EXISTS forgevcs_workspace_lease_events (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id   UUID NOT NULL,
    repo           TEXT NOT NULL,
    event          TEXT NOT NULL,
    owner_id       TEXT NOT NULL,
    prior_owner_id TEXT,
    lease_id       UUID NOT NULL,
    at             TIMESTAMPTZ NOT NULL,
    detail         JSONB
);
CREATE INDEX IF NOT EXISTS forgevcs_workspace_lease_events_workspace
    ON forgevcs_workspace_lease_events (workspace_id, at DESC);
CREATE INDEX IF NOT EXISTS forgevcs_workspace_lease_events_repo
    ON forgevcs_workspace_lease_events (repo, at DESC);
"#;

/// Create the lease and lease-audit tables when they do not exist. A failure is surfaced to the
/// boot path, which logs it; lease-dependent endpoints then answer 503.
pub async fn ensure_schema(pool: &Pool) -> Result<(), String> {
    let client = pool.get().await.map_err(|error| error.to_string())?;
    client
        .batch_execute(CREATE_LEASES)
        .await
        .map_err(|error| format!("creating workspace lease tables: {error}"))
}

/// Wall-clock seconds since the Unix epoch. Times before the epoch saturate to zero (the service
/// cannot be given such a clock in practice; tests use it for determinism).
pub fn unix_secs(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// What happened on the lease, as written to `forgevcs_workspace_lease_events`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseEventKind {
    /// A writer acquired a free workspace.
    Acquired,
    /// A writer released the lease (submit/destroy).
    Released,
    /// A writer took over a stale (expired) lease from another owner.
    Takeover,
}

impl LeaseEventKind {
    /// The stable `event` slug stored in Postgres.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Acquired => "acquired",
            Self::Released => "released",
            Self::Takeover => "takeover",
        }
    }
}

/// A lease held by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseGrant {
    /// Workspace the lease protects.
    pub workspace_id: Uuid,
    /// Repository the workspace belongs to.
    pub repo: String,
    /// Principal that owns the lease (`service:agent-a`, `github:login`).
    pub owner: String,
    /// Fencing token; changes when the lease changes owner, stable across refreshes.
    pub lease_id: Uuid,
    /// Unix seconds when this ownership started.
    pub acquired_at_unix: i64,
    /// Unix seconds of the last heartbeat.
    pub heartbeat_at_unix: i64,
    /// Unix seconds after which the lease is stale.
    pub expires_at_unix: i64,
    /// Present when this acquisition took over an expired lease.
    pub takeover: Option<Takeover>,
}

/// Evidence that an acquisition displaced a stale lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Takeover {
    /// Principal whose lease expired.
    pub prior_owner: String,
    /// That lease's fencing token.
    pub prior_lease_id: Uuid,
    /// That lease's expiry (Unix seconds).
    pub prior_expires_at_unix: i64,
}

/// The current lease on a workspace, reported to a writer that lost the race.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseConflict {
    /// Workspace that is leased.
    pub workspace_id: Uuid,
    /// Repository of the workspace.
    pub repo: String,
    /// Current owner of the lease.
    pub owner: String,
    /// Current fencing token.
    pub lease_id: Uuid,
    /// Unix seconds when the current lease expires (takeover becomes possible after this).
    pub expires_at_unix: i64,
}

/// Failure of a lease operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseError {
    /// Another live owner holds the lease; the caller must not write.
    Conflict(LeaseConflict),
    /// The workspace (or its lease) no longer exists.
    NotFound(String),
    /// The database is not reachable right now; the caller may retry.
    Unavailable(String),
    /// The lease invariant of the service itself failed.
    Internal(String),
}

impl LeaseError {
    /// Convert to the internal gRPC status the workspace layer returns. A conflict becomes
    /// `Aborted` (HTTP 409) with the current owner in metadata; the façade turns that metadata
    /// into the typed `workspace_lease_conflict` JSON error (see `api_error`).
    pub fn into_status(self) -> Status {
        match self {
            Self::Conflict(conflict) => {
                let mut status = Status::aborted(format!(
                    "workspace {} is leased by {} until unix {}",
                    conflict.workspace_id, conflict.owner, conflict.expires_at_unix
                ));
                put_metadata(status.metadata_mut(), METADATA_OWNER, &conflict.owner);
                put_metadata(
                    status.metadata_mut(),
                    METADATA_LEASE_ID,
                    &conflict.lease_id.to_string(),
                );
                put_metadata(
                    status.metadata_mut(),
                    METADATA_WORKSPACE_ID,
                    &conflict.workspace_id.to_string(),
                );
                put_metadata(
                    status.metadata_mut(),
                    METADATA_EXPIRES_AT,
                    &conflict.expires_at_unix.to_string(),
                );
                put_metadata(status.metadata_mut(), METADATA_REPO, &conflict.repo);
                status
            }
            Self::NotFound(message) => Status::not_found(message),
            Self::Unavailable(message) => Status::unavailable(message),
            Self::Internal(message) => Status::internal(message),
        }
    }
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(conflict) => write!(
                formatter,
                "workspace {} is leased by {} until unix {}",
                conflict.workspace_id, conflict.owner, conflict.expires_at_unix
            ),
            Self::NotFound(message) | Self::Unavailable(message) | Self::Internal(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for LeaseError {}

fn put_metadata(metadata: &mut MetadataMap, key: &'static str, value: &str) {
    if let Ok(value) = tonic::metadata::MetadataValue::try_from(value) {
        metadata.insert(key, value);
    }
}

/// An acquire request. `lease_id` is the fencing token the store should use if it grants a new
/// lease (free workspace or takeover); a same-owner refresh keeps the stored token.
#[derive(Debug, Clone, Copy)]
pub struct AcquireRequest<'a> {
    /// Workspace to lease.
    pub workspace_id: Uuid,
    /// Repository of the workspace.
    pub repo: &'a str,
    /// Principal requesting the lease.
    pub owner: &'a str,
    /// Proposed fencing token for a new lease.
    pub lease_id: Uuid,
    /// Wall clock, Unix seconds.
    pub now_unix: i64,
    /// Lease lifetime from `now_unix`, seconds.
    pub ttl_secs: i64,
}

/// A heartbeat request against an already granted lease.
#[derive(Debug, Clone, Copy)]
pub struct HeartbeatRequest<'a> {
    /// Workspace of the lease.
    pub workspace_id: Uuid,
    /// Repository of the workspace.
    pub repo: &'a str,
    /// Principal that believes it holds the lease.
    pub owner: &'a str,
    /// Fencing token it holds.
    pub lease_id: Uuid,
    /// Wall clock, Unix seconds.
    pub now_unix: i64,
    /// New lifetime from `now_unix`, seconds.
    pub ttl_secs: i64,
}

/// A release request against an already granted lease.
#[derive(Debug, Clone, Copy)]
pub struct ReleaseRequest<'a> {
    /// Workspace of the lease.
    pub workspace_id: Uuid,
    /// Repository of the workspace.
    pub repo: &'a str,
    /// Principal releasing the lease.
    pub owner: &'a str,
    /// Fencing token it holds.
    pub lease_id: Uuid,
    /// Wall clock, Unix seconds (recorded on the audit row).
    pub now_unix: i64,
}

type LeaseFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, LeaseError>> + Send + 'a>>;

/// Storage for leases and their audit trail. Production uses [`PgLeaseStore`]; the in-memory
/// implementation below exists so the concurrency/expiry/takeover semantics can be tested without
/// a database.
pub trait LeaseStore: Send + Sync {
    /// Acquire (or refresh/take over) the lease on `request.workspace_id`.
    fn acquire<'a>(&'a self, request: AcquireRequest<'a>) -> LeaseFuture<'a, LeaseGrant>;

    /// Renew a held lease; fails with a conflict when the lease is no longer ours.
    fn heartbeat<'a>(&'a self, request: HeartbeatRequest<'a>) -> LeaseFuture<'a, LeaseGrant>;

    /// Release a held lease. Idempotent: releasing an already-free lease returns `false`.
    fn release<'a>(&'a self, request: ReleaseRequest<'a>) -> LeaseFuture<'a, bool>;

    /// Record a release after the workspace row (and therefore the lease row) was destroyed.
    fn record_released<'a>(&'a self, request: ReleaseRequest<'a>) -> LeaseFuture<'a, ()>;
}

/// The per-process lease service: TTL policy plus the store, used by [`crate::workspace`].
#[derive(Clone)]
pub struct WorkspaceLease {
    store: std::sync::Arc<dyn LeaseStore>,
    ttl_secs: i64,
}

impl std::fmt::Debug for WorkspaceLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceLease")
            .field("ttl_secs", &self.ttl_secs)
            .finish_non_exhaustive()
    }
}

impl WorkspaceLease {
    /// Build the service over a store and a lease lifetime.
    pub fn new(store: std::sync::Arc<dyn LeaseStore>, ttl: Duration) -> Self {
        let ttl_secs = i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
        Self {
            store,
            ttl_secs: ttl_secs.max(1),
        }
    }

    /// The configured lease lifetime.
    pub fn ttl(&self) -> Duration {
        Duration::from_secs(u64::try_from(self.ttl_secs).unwrap_or(u64::MAX))
    }

    /// Acquire the lease for `owner`, refreshing it when the same owner already holds it,
    /// taking it over when it is stale, and conflicting when a foreign owner holds a live lease.
    pub async fn acquire(
        &self,
        workspace_id: Uuid,
        repo: &str,
        owner: &str,
        now: SystemTime,
    ) -> Result<LeaseGrant, LeaseError> {
        self.store
            .acquire(AcquireRequest {
                workspace_id,
                repo,
                owner,
                lease_id: Uuid::new_v4(),
                now_unix: unix_secs(now),
                ttl_secs: self.ttl_secs,
            })
            .await
    }

    /// Renew a lease this caller holds.
    pub async fn heartbeat(
        &self,
        grant: &LeaseGrant,
        now: SystemTime,
    ) -> Result<LeaseGrant, LeaseError> {
        self.store
            .heartbeat(HeartbeatRequest {
                workspace_id: grant.workspace_id,
                repo: &grant.repo,
                owner: &grant.owner,
                lease_id: grant.lease_id,
                now_unix: unix_secs(now),
                ttl_secs: self.ttl_secs,
            })
            .await
    }

    /// Release a lease this caller holds.
    pub async fn release(&self, grant: &LeaseGrant, now: SystemTime) -> Result<bool, LeaseError> {
        self.store
            .release(ReleaseRequest {
                workspace_id: grant.workspace_id,
                repo: &grant.repo,
                owner: &grant.owner,
                lease_id: grant.lease_id,
                now_unix: unix_secs(now),
            })
            .await
    }

    /// Record a release after the workspace (and its lease row) was destroyed.
    pub async fn record_released(
        &self,
        grant: &LeaseGrant,
        now: SystemTime,
    ) -> Result<(), LeaseError> {
        self.store
            .record_released(ReleaseRequest {
                workspace_id: grant.workspace_id,
                repo: &grant.repo,
                owner: &grant.owner,
                lease_id: grant.lease_id,
                now_unix: unix_secs(now),
            })
            .await
    }
}

/// Postgres-backed lease store. One transaction per decision; see the module docs.
#[derive(Debug, Clone)]
pub struct PgLeaseStore {
    pool: Pool,
}

impl PgLeaseStore {
    /// Build a store over the shared ForgeVCS-owned Postgres pool.
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    async fn client(&self) -> Result<deadpool_postgres::Client, LeaseError> {
        self.pool.get().await.map_err(|error| {
            LeaseError::Unavailable(format!("workspace lease store is unavailable: {error}"))
        })
    }
}

fn db_error(context: &str, error: tokio_postgres::Error) -> LeaseError {
    if let Some(code) = error.code()
        && *code == tokio_postgres::error::SqlState::FOREIGN_KEY_VIOLATION
    {
        return LeaseError::NotFound("workspace no longer exists".to_owned());
    }
    LeaseError::Internal(format!("{context}: {error}"))
}

fn seconds(value: i64) -> f64 {
    value as f64
}

// The audit row has one field per column and every caller has them in scope, so an argument
// struct would only move the list. The codebase uses the same allow for its wide `record` helpers.
#[allow(clippy::too_many_arguments)]
async fn insert_event(
    transaction: &deadpool_postgres::Transaction<'_>,
    workspace_id: Uuid,
    repo: &str,
    kind: LeaseEventKind,
    owner: &str,
    prior_owner: Option<&str>,
    lease_id: Uuid,
    now_unix: i64,
    detail: &Value,
) -> Result<(), tokio_postgres::Error> {
    transaction
        .execute(
            "INSERT INTO forgevcs_workspace_lease_events \
             (workspace_id, repo, event, owner_id, prior_owner_id, lease_id, at, detail) \
             VALUES ($1, $2, $3, $4, $5, $6, to_timestamp($7), $8::jsonb)",
            &[
                &workspace_id,
                &repo,
                &kind.as_str(),
                &owner,
                &prior_owner,
                &lease_id,
                &seconds(now_unix),
                &detail,
            ],
        )
        .await
        .map(|_| ())
}

#[derive(Debug, Clone)]
struct StoredLease {
    repo: String,
    owner: String,
    lease_id: Uuid,
    acquired_at_unix: i64,
    heartbeat_at_unix: i64,
    expires_at_unix: i64,
}

fn grant_from_stored(workspace_id: Uuid, stored: StoredLease) -> LeaseGrant {
    LeaseGrant {
        workspace_id,
        repo: stored.repo,
        owner: stored.owner,
        lease_id: stored.lease_id,
        acquired_at_unix: stored.acquired_at_unix,
        heartbeat_at_unix: stored.heartbeat_at_unix,
        expires_at_unix: stored.expires_at_unix,
        takeover: None,
    }
}

fn conflict_from_stored(workspace_id: Uuid, stored: StoredLease) -> LeaseError {
    LeaseError::Conflict(LeaseConflict {
        workspace_id,
        repo: stored.repo,
        owner: stored.owner,
        lease_id: stored.lease_id,
        expires_at_unix: stored.expires_at_unix,
    })
}

impl LeaseStore for PgLeaseStore {
    fn acquire<'a>(&'a self, request: AcquireRequest<'a>) -> LeaseFuture<'a, LeaseGrant> {
        Box::pin(async move {
            let mut client = self.client().await?;
            let transaction = client
                .transaction()
                .await
                .map_err(|error| db_error("starting lease transaction", error))?;

            // First writer inserts; everyone else is serialised by the primary key. ON CONFLICT
            // DO NOTHING waits for a concurrent insert to commit, so the SELECT below sees the
            // winner's row; SELECT … FOR UPDATE then serialises refresh/takeover/conflict. The
            // affected-row count tells us whether this transaction created the lease, which is
            // the `acquired` audit event (a same-owner refresh is not an acquisition).
            let expiry_unix = request.now_unix.saturating_add(request.ttl_secs.max(1));
            let inserted = transaction
                .execute(
                    "INSERT INTO forgevcs_workspace_leases \
                     (workspace_id, repo, owner_id, lease_id, acquired_at, heartbeat_at, expires_at) \
                     VALUES ($1, $2, $3, $4, to_timestamp($5), to_timestamp($5), to_timestamp($6)) \
                     ON CONFLICT (workspace_id) DO NOTHING",
                    &[
                        &request.workspace_id,
                        &request.repo,
                        &request.owner,
                        &request.lease_id,
                        &seconds(request.now_unix),
                        &seconds(expiry_unix),
                    ],
                )
                .await
                .map_err(|error| db_error("inserting workspace lease", error))?;

            let row = transaction
                .query_opt(
                    "SELECT repo, owner_id, lease_id, \
                            EXTRACT(EPOCH FROM acquired_at)::bigint AS acquired_at_unix, \
                            EXTRACT(EPOCH FROM heartbeat_at)::bigint AS heartbeat_at_unix, \
                            EXTRACT(EPOCH FROM expires_at)::bigint AS expires_at_unix, \
                            (expires_at <= to_timestamp($2)) AS expired \
                     FROM forgevcs_workspace_leases WHERE workspace_id = $1 FOR UPDATE",
                    &[&request.workspace_id, &seconds(request.now_unix)],
                )
                .await
                .map_err(|error| db_error("reading workspace lease", error))?;

            let Some(row) = row else {
                // The INSERT succeeded above, so this can only happen if the workspace row was
                // deleted between the FK check and now; the insert would have failed instead.
                let _ = transaction.rollback().await;
                return Err(LeaseError::NotFound(
                    "workspace no longer exists".to_owned(),
                ));
            };
            let stored = StoredLease {
                repo: row.get("repo"),
                owner: row.get("owner_id"),
                lease_id: row.get("lease_id"),
                acquired_at_unix: row.get("acquired_at_unix"),
                heartbeat_at_unix: row.get("heartbeat_at_unix"),
                expires_at_unix: row.get("expires_at_unix"),
            };

            if stored.owner == request.owner {
                // Same owner: renew in place, keep the fencing token and acquisition time. A row
                // this transaction inserted is an acquisition; a refresh is not a new event.
                transaction
                    .execute(
                        "UPDATE forgevcs_workspace_leases \
                         SET heartbeat_at = to_timestamp($2), expires_at = to_timestamp($3), repo = $4 \
                         WHERE workspace_id = $1",
                        &[
                            &request.workspace_id,
                            &seconds(request.now_unix),
                            &seconds(expiry_unix),
                            &request.repo,
                        ],
                    )
                    .await
                    .map_err(|error| db_error("refreshing workspace lease", error))?;
                if inserted == 1 {
                    let detail = json!({});
                    insert_event(
                        &transaction,
                        request.workspace_id,
                        request.repo,
                        LeaseEventKind::Acquired,
                        request.owner,
                        None,
                        request.lease_id,
                        request.now_unix,
                        &detail,
                    )
                    .await
                    .map_err(|error| db_error("recording workspace lease acquisition", error))?;
                }
                transaction
                    .commit()
                    .await
                    .map_err(|error| db_error("committing workspace lease refresh", error))?;
                let mut grant = grant_from_stored(request.workspace_id, stored);
                grant.heartbeat_at_unix = request.now_unix;
                grant.expires_at_unix = expiry_unix;
                return Ok(grant);
            }

            let expired = row.get::<_, bool>("expired");
            if !expired {
                let _ = transaction.rollback().await;
                return Err(conflict_from_stored(request.workspace_id, stored));
            }

            // Stale lease: take it over and write the audit record in the same transaction.
            transaction
                .execute(
                    "UPDATE forgevcs_workspace_leases \
                     SET repo = $2, owner_id = $3, lease_id = $4, \
                         acquired_at = to_timestamp($5), heartbeat_at = to_timestamp($5), \
                         expires_at = to_timestamp($6) \
                     WHERE workspace_id = $1",
                    &[
                        &request.workspace_id,
                        &request.repo,
                        &request.owner,
                        &request.lease_id,
                        &seconds(request.now_unix),
                        &seconds(expiry_unix),
                    ],
                )
                .await
                .map_err(|error| db_error("taking over workspace lease", error))?;
            let detail = json!({
                "prior_expires_at_unix": stored.expires_at_unix,
                "prior_lease_id": stored.lease_id.to_string(),
                "prior_owner": stored.owner,
            });
            insert_event(
                &transaction,
                request.workspace_id,
                request.repo,
                LeaseEventKind::Takeover,
                request.owner,
                Some(&stored.owner),
                request.lease_id,
                request.now_unix,
                &detail,
            )
            .await
            .map_err(|error| db_error("recording workspace lease takeover", error))?;
            transaction
                .commit()
                .await
                .map_err(|error| db_error("committing workspace lease takeover", error))?;

            Ok(LeaseGrant {
                workspace_id: request.workspace_id,
                repo: request.repo.to_owned(),
                owner: request.owner.to_owned(),
                lease_id: request.lease_id,
                acquired_at_unix: request.now_unix,
                heartbeat_at_unix: request.now_unix,
                expires_at_unix: expiry_unix,
                takeover: Some(Takeover {
                    prior_owner: stored.owner,
                    prior_lease_id: stored.lease_id,
                    prior_expires_at_unix: stored.expires_at_unix,
                }),
            })
        })
    }

    fn heartbeat<'a>(&'a self, request: HeartbeatRequest<'a>) -> LeaseFuture<'a, LeaseGrant> {
        Box::pin(async move {
            let client = self.client().await?;
            let expiry_unix = request.now_unix.saturating_add(request.ttl_secs.max(1));
            let updated = client
                .query_opt(
                    "UPDATE forgevcs_workspace_leases \
                     SET heartbeat_at = to_timestamp($4), expires_at = to_timestamp($5) \
                     WHERE workspace_id = $1 AND owner_id = $2 AND lease_id = $3 \
                     RETURNING repo, owner_id, lease_id, \
                               EXTRACT(EPOCH FROM acquired_at)::bigint AS acquired_at_unix, \
                               EXTRACT(EPOCH FROM heartbeat_at)::bigint AS heartbeat_at_unix, \
                               EXTRACT(EPOCH FROM expires_at)::bigint AS expires_at_unix",
                    &[
                        &request.workspace_id,
                        &request.owner,
                        &request.lease_id,
                        &seconds(request.now_unix),
                        &seconds(expiry_unix),
                    ],
                )
                .await
                .map_err(|error| db_error("heartbeating workspace lease", error))?;
            if let Some(row) = updated {
                return Ok(grant_from_stored(
                    request.workspace_id,
                    StoredLease {
                        repo: row.get("repo"),
                        owner: row.get("owner_id"),
                        lease_id: row.get("lease_id"),
                        acquired_at_unix: row.get("acquired_at_unix"),
                        heartbeat_at_unix: row.get("heartbeat_at_unix"),
                        expires_at_unix: row.get("expires_at_unix"),
                    },
                ));
            }
            // The lease is no longer ours: report who holds it (or that it is gone).
            let row = client
                .query_opt(
                    "SELECT repo, owner_id, lease_id, \
                            EXTRACT(EPOCH FROM acquired_at)::bigint AS acquired_at_unix, \
                            EXTRACT(EPOCH FROM heartbeat_at)::bigint AS heartbeat_at_unix, \
                            EXTRACT(EPOCH FROM expires_at)::bigint AS expires_at_unix \
                     FROM forgevcs_workspace_leases WHERE workspace_id = $1",
                    &[&request.workspace_id],
                )
                .await
                .map_err(|error| db_error("reading workspace lease after heartbeat", error))?;
            match row {
                Some(row) => Err(conflict_from_stored(
                    request.workspace_id,
                    StoredLease {
                        repo: row.get("repo"),
                        owner: row.get("owner_id"),
                        lease_id: row.get("lease_id"),
                        acquired_at_unix: row.get("acquired_at_unix"),
                        heartbeat_at_unix: row.get("heartbeat_at_unix"),
                        expires_at_unix: row.get("expires_at_unix"),
                    },
                )),
                None => Err(LeaseError::NotFound(
                    "workspace lease no longer exists".to_owned(),
                )),
            }
        })
    }

    fn release<'a>(&'a self, request: ReleaseRequest<'a>) -> LeaseFuture<'a, bool> {
        Box::pin(async move {
            let mut client = self.client().await?;
            let transaction = client
                .transaction()
                .await
                .map_err(|error| db_error("starting lease release transaction", error))?;
            let deleted = transaction
                .query_opt(
                    "DELETE FROM forgevcs_workspace_leases \
                     WHERE workspace_id = $1 AND owner_id = $2 AND lease_id = $3 \
                     RETURNING repo",
                    &[&request.workspace_id, &request.owner, &request.lease_id],
                )
                .await
                .map_err(|error| db_error("releasing workspace lease", error))?;
            let Some(row) = deleted else {
                let _ = transaction.rollback().await;
                return Ok(false);
            };
            let repo: String = row.get("repo");
            let detail = json!({ "reason": "submit" });
            insert_event(
                &transaction,
                request.workspace_id,
                &repo,
                LeaseEventKind::Released,
                request.owner,
                None,
                request.lease_id,
                request.now_unix,
                &detail,
            )
            .await
            .map_err(|error| db_error("recording workspace lease release", error))?;
            transaction
                .commit()
                .await
                .map_err(|error| db_error("committing workspace lease release", error))?;
            Ok(true)
        })
    }

    fn record_released<'a>(&'a self, request: ReleaseRequest<'a>) -> LeaseFuture<'a, ()> {
        Box::pin(async move {
            let mut client = self.client().await?;
            let transaction = client
                .transaction()
                .await
                .map_err(|error| db_error("starting lease release transaction", error))?;
            let detail = json!({ "reason": "destroy" });
            insert_event(
                &transaction,
                request.workspace_id,
                request.repo,
                LeaseEventKind::Released,
                request.owner,
                None,
                request.lease_id,
                request.now_unix,
                &detail,
            )
            .await
            .map_err(|error| db_error("recording workspace lease release after destroy", error))?;
            transaction
                .commit()
                .await
                .map_err(|error| db_error("committing workspace lease release", error))?;
            Ok(())
        })
    }
}

/// In-memory store with the same contract as [`PgLeaseStore`], for tests. Kept test-only: the
/// deployed service always uses Postgres, so this cannot become a second production path.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct MemoryLeaseStore {
    inner: Mutex<MemoryState>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct MemoryState {
    leases: HashMap<Uuid, StoredLease>,
    events: Vec<LeaseAudit>,
}

/// One audited lease event, as the in-memory store records it.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseAudit {
    /// Workspace the event is about.
    pub workspace_id: Uuid,
    /// Repository of the workspace.
    pub repo: String,
    /// Event kind.
    pub event: LeaseEventKind,
    /// Principal that acted.
    pub owner: String,
    /// Prior owner on a takeover.
    pub prior_owner: Option<String>,
    /// Lease token the event concerns.
    pub lease_id: Uuid,
    /// Wall clock of the event, Unix seconds.
    pub at_unix: i64,
    /// Event detail.
    pub detail: Value,
}

#[cfg(test)]
impl MemoryLeaseStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, MemoryState> {
        match self.inner.lock() {
            Ok(guard) => guard,
            // A failed assertion must not turn every later lock into a second panic.
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// The audit trail for one workspace, in order.
    pub fn audit(&self, workspace_id: Uuid) -> Vec<LeaseAudit> {
        self.state()
            .events
            .iter()
            .filter(|event| event.workspace_id == workspace_id)
            .cloned()
            .collect()
    }

    /// The current lease row for a workspace, when held.
    pub fn current(&self, workspace_id: Uuid) -> Option<(String, Uuid, i64)> {
        self.state()
            .leases
            .get(&workspace_id)
            .map(|lease| (lease.owner.clone(), lease.lease_id, lease.expires_at_unix))
    }
}

#[cfg(test)]
fn push_event(state: &mut MemoryState, request: &AcquireRequest<'_>, prior: Option<&StoredLease>) {
    let (event, owner, lease_id, detail) = match prior {
        Some(prior) => (
            LeaseEventKind::Takeover,
            request.owner,
            request.lease_id,
            json!({
                "prior_expires_at_unix": prior.expires_at_unix,
                "prior_lease_id": prior.lease_id.to_string(),
                "prior_owner": prior.owner,
            }),
        ),
        None => (
            LeaseEventKind::Acquired,
            request.owner,
            request.lease_id,
            json!({}),
        ),
    };
    state.events.push(LeaseAudit {
        workspace_id: request.workspace_id,
        repo: request.repo.to_owned(),
        event,
        owner: owner.to_owned(),
        prior_owner: prior.map(|lease| lease.owner.clone()),
        lease_id,
        at_unix: request.now_unix,
        detail,
    });
}

#[cfg(test)]
impl LeaseStore for MemoryLeaseStore {
    fn acquire<'a>(&'a self, request: AcquireRequest<'a>) -> LeaseFuture<'a, LeaseGrant> {
        Box::pin(async move {
            let mut state = self.state();
            let expiry_unix = request.now_unix.saturating_add(request.ttl_secs.max(1));
            if let Some(existing) = state.leases.get(&request.workspace_id).cloned() {
                if existing.owner == request.owner {
                    let mut refreshed = existing;
                    refreshed.repo = request.repo.to_owned();
                    refreshed.heartbeat_at_unix = request.now_unix;
                    refreshed.expires_at_unix = expiry_unix;
                    state.leases.insert(request.workspace_id, refreshed.clone());
                    return Ok(grant_from_stored(request.workspace_id, refreshed));
                }
                if existing.expires_at_unix > request.now_unix {
                    return Err(conflict_from_stored(request.workspace_id, existing));
                }
                let takeover = Takeover {
                    prior_owner: existing.owner.clone(),
                    prior_lease_id: existing.lease_id,
                    prior_expires_at_unix: existing.expires_at_unix,
                };
                let prior = existing;
                let stored = StoredLease {
                    repo: request.repo.to_owned(),
                    owner: request.owner.to_owned(),
                    lease_id: request.lease_id,
                    acquired_at_unix: request.now_unix,
                    heartbeat_at_unix: request.now_unix,
                    expires_at_unix: expiry_unix,
                };
                state.leases.insert(request.workspace_id, stored.clone());
                push_event(&mut state, &request, Some(&prior));
                let mut grant = grant_from_stored(request.workspace_id, stored);
                grant.takeover = Some(takeover);
                return Ok(grant);
            }
            let stored = StoredLease {
                repo: request.repo.to_owned(),
                owner: request.owner.to_owned(),
                lease_id: request.lease_id,
                acquired_at_unix: request.now_unix,
                heartbeat_at_unix: request.now_unix,
                expires_at_unix: expiry_unix,
            };
            state.leases.insert(request.workspace_id, stored.clone());
            push_event(&mut state, &request, None);
            Ok(grant_from_stored(request.workspace_id, stored))
        })
    }

    fn heartbeat<'a>(&'a self, request: HeartbeatRequest<'a>) -> LeaseFuture<'a, LeaseGrant> {
        Box::pin(async move {
            let mut state = self.state();
            let expiry_unix = request.now_unix.saturating_add(request.ttl_secs.max(1));
            match state.leases.get_mut(&request.workspace_id) {
                None => Err(LeaseError::NotFound(
                    "workspace lease no longer exists".to_owned(),
                )),
                Some(lease)
                    if lease.owner == request.owner && lease.lease_id == request.lease_id =>
                {
                    lease.repo = request.repo.to_owned();
                    lease.heartbeat_at_unix = request.now_unix;
                    lease.expires_at_unix = expiry_unix;
                    Ok(grant_from_stored(
                        request.workspace_id,
                        lease.clone_shallow(),
                    ))
                }
                Some(lease) => Err(conflict_from_stored(
                    request.workspace_id,
                    lease.clone_shallow(),
                )),
            }
        })
    }

    fn release<'a>(&'a self, request: ReleaseRequest<'a>) -> LeaseFuture<'a, bool> {
        Box::pin(async move {
            let mut state = self.state();
            let owned = state
                .leases
                .get(&request.workspace_id)
                .is_some_and(|lease| {
                    lease.owner == request.owner && lease.lease_id == request.lease_id
                });
            if !owned {
                return Ok(false);
            }
            let removed = state.leases.remove(&request.workspace_id);
            state.events.push(LeaseAudit {
                workspace_id: request.workspace_id,
                repo: request.repo.to_owned(),
                event: LeaseEventKind::Released,
                owner: request.owner.to_owned(),
                prior_owner: None,
                lease_id: request.lease_id,
                at_unix: request.now_unix,
                detail: json!({ "reason": "submit" }),
            });
            // `removed` is intentionally unused: the audit row is the record.
            drop(removed);
            Ok(true)
        })
    }

    fn record_released<'a>(&'a self, request: ReleaseRequest<'a>) -> LeaseFuture<'a, ()> {
        Box::pin(async move {
            self.state().events.push(LeaseAudit {
                workspace_id: request.workspace_id,
                repo: request.repo.to_owned(),
                event: LeaseEventKind::Released,
                owner: request.owner.to_owned(),
                prior_owner: None,
                lease_id: request.lease_id,
                at_unix: request.now_unix,
                detail: json!({ "reason": "destroy" }),
            });
            Ok(())
        })
    }
}

#[cfg(test)]
impl StoredLease {
    /// The in-memory store keeps the row behind one lock; grant/conflict builders only need the
    /// fields, so clone them explicitly rather than exposing the map's ownership.
    fn clone_shallow(&self) -> Self {
        Self {
            repo: self.repo.clone(),
            owner: self.owner.clone(),
            lease_id: self.lease_id,
            acquired_at_unix: self.acquired_at_unix,
            heartbeat_at_unix: self.heartbeat_at_unix,
            expires_at_unix: self.expires_at_unix,
        }
    }
}

/// The `tonic` status code a lease conflict must map to. A test asserts that the façade's
/// `ApiError` mapping turns it into the typed 409.
pub fn is_lease_conflict(status: &Status) -> bool {
    status.code() == Code::Aborted && status.metadata().get(METADATA_OWNER).is_some()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::Arc;

    use super::*;

    const REPO: &str = "lease-test-repo";
    const TTL: Duration = Duration::from_secs(300);

    fn t0() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn at(offset_secs: u64) -> SystemTime {
        t0() + Duration::from_secs(offset_secs)
    }

    fn service() -> (Arc<MemoryLeaseStore>, WorkspaceLease) {
        let store = Arc::new(MemoryLeaseStore::new());
        let lease = WorkspaceLease::new(store.clone(), TTL);
        (store, lease)
    }

    #[test]
    fn the_default_ttl_is_five_minutes() {
        assert_eq!(DEFAULT_TTL_SECS, 300);
        assert_eq!(
            WorkspaceLease::new(Arc::new(MemoryLeaseStore::new()), Duration::from_secs(300)).ttl(),
            Duration::from_secs(300)
        );
    }

    #[tokio::test]
    async fn a_free_workspace_is_acquired_and_audited() {
        let (store, lease) = service();
        let workspace = Uuid::new_v4();
        let grant = lease
            .acquire(workspace, REPO, "service:agent-a", t0())
            .await
            .unwrap();
        assert_eq!(grant.owner, "service:agent-a");
        assert_eq!(grant.repo, REPO);
        assert_eq!(grant.expires_at_unix, unix_secs(t0()) + 300);
        assert!(grant.takeover.is_none());
        let audit = store.audit(workspace);
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].event, LeaseEventKind::Acquired);
        assert_eq!(audit[0].owner, "service:agent-a");
        assert_eq!(audit[0].lease_id, grant.lease_id);
    }

    /// R4 acceptance: two writers racing for the same workspace produce exactly one winner; every
    /// loser gets a typed conflict naming the winner and its fencing token.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writers_one_wins() {
        let (store, lease) = service();
        let lease = Arc::new(lease);
        let workspace = Uuid::new_v4();
        let writers = 8usize;
        let barrier = Arc::new(tokio::sync::Barrier::new(writers));
        let mut tasks = Vec::with_capacity(writers);
        for index in 0..writers {
            let lease = Arc::clone(&lease);
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                let owner = format!("service:agent-{index}");
                barrier.wait().await;
                lease.acquire(workspace, REPO, &owner, t0()).await
            }));
        }
        let mut wins = Vec::new();
        let mut conflicts = Vec::new();
        for task in tasks {
            match task.await.unwrap() {
                Ok(grant) => wins.push(grant),
                Err(LeaseError::Conflict(conflict)) => conflicts.push(conflict),
                Err(other) => panic!("unexpected lease error: {other:?}"),
            }
        }
        assert_eq!(wins.len(), 1, "exactly one writer may win");
        assert_eq!(conflicts.len(), writers - 1);
        let winner = &wins[0];
        for conflict in conflicts {
            assert_eq!(conflict.workspace_id, workspace);
            assert_eq!(conflict.owner, winner.owner);
            assert_eq!(conflict.lease_id, winner.lease_id);
            assert_eq!(conflict.expires_at_unix, winner.expires_at_unix);
        }
        // Only the winner's acquisition is audited; conflicts never mutate the lease.
        let audit = store.audit(workspace);
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].owner, winner.owner);
    }

    /// R4 acceptance: an expired lease can be taken over, and the takeover names the prior owner
    /// in the audit trail.
    #[tokio::test]
    async fn an_expired_lease_is_taken_over_with_audit() {
        let (store, lease) = service();
        let workspace = Uuid::new_v4();
        let first = lease
            .acquire(workspace, REPO, "service:agent-a", t0())
            .await
            .unwrap();
        // At ttl-1 the lease is still live.
        let refused = lease
            .acquire(workspace, REPO, "service:agent-b", at(299))
            .await;
        assert!(matches!(refused, Err(LeaseError::Conflict(_))));
        // At ttl it is stale; takeover succeeds and records prior ownership.
        let second = lease
            .acquire(workspace, REPO, "service:agent-b", at(300))
            .await
            .unwrap();
        assert_eq!(second.owner, "service:agent-b");
        assert_ne!(
            second.lease_id, first.lease_id,
            "takeover rotates the token"
        );
        let takeover = second.takeover.expect("takeover evidence");
        assert_eq!(takeover.prior_owner, "service:agent-a");
        assert_eq!(takeover.prior_lease_id, first.lease_id);
        assert_eq!(takeover.prior_expires_at_unix, first.expires_at_unix);
        let audit = store.audit(workspace);
        assert_eq!(audit.len(), 2);
        assert_eq!(audit[1].event, LeaseEventKind::Takeover);
        assert_eq!(audit[1].owner, "service:agent-b");
        assert_eq!(audit[1].prior_owner.as_deref(), Some("service:agent-a"));
        assert_eq!(audit[1].detail["prior_owner"], "service:agent-a");
    }

    /// R4 acceptance: release lets the next writer acquire immediately, with no takeover.
    #[tokio::test]
    async fn release_allows_the_next_writer() {
        let (store, lease) = service();
        let workspace = Uuid::new_v4();
        let first = lease
            .acquire(workspace, REPO, "service:agent-a", t0())
            .await
            .unwrap();
        assert!(lease.release(&first, at(1)).await.unwrap());
        let second = lease
            .acquire(workspace, REPO, "service:agent-b", at(2))
            .await
            .unwrap();
        assert_eq!(second.owner, "service:agent-b");
        assert!(
            second.takeover.is_none(),
            "a released lease is free, not stale"
        );
        let audit = store.audit(workspace);
        let kinds: Vec<_> = audit.iter().map(|event| event.event).collect();
        assert_eq!(
            kinds,
            vec![
                LeaseEventKind::Acquired,
                LeaseEventKind::Released,
                LeaseEventKind::Acquired
            ]
        );
    }

    #[tokio::test]
    async fn release_is_idempotent_and_only_the_owner_can_release() {
        let (store, lease) = service();
        let workspace = Uuid::new_v4();
        let grant = lease
            .acquire(workspace, REPO, "service:agent-a", t0())
            .await
            .unwrap();
        // A foreign owner quoting the token cannot release it.
        let foreign = store
            .release(ReleaseRequest {
                workspace_id: workspace,
                repo: REPO,
                owner: "service:agent-b",
                lease_id: grant.lease_id,
                now_unix: unix_secs(at(1)),
            })
            .await
            .unwrap();
        assert!(!foreign);
        assert_eq!(
            store.current(workspace).unwrap().0,
            "service:agent-a",
            "the owner still holds the lease after a foreign release"
        );
        assert!(lease.release(&grant, at(2)).await.unwrap());
        assert!(!lease.release(&grant, at(3)).await.unwrap());
    }

    #[tokio::test]
    async fn same_owner_acquisition_is_a_heartbeat_not_a_conflict() {
        let (_store, lease) = service();
        let workspace = Uuid::new_v4();
        let first = lease
            .acquire(workspace, REPO, "service:agent-a", t0())
            .await
            .unwrap();
        let refreshed = lease
            .acquire(workspace, REPO, "service:agent-a", at(100))
            .await
            .unwrap();
        assert_eq!(refreshed.lease_id, first.lease_id, "token is stable");
        assert_eq!(refreshed.acquired_at_unix, first.acquired_at_unix);
        assert_eq!(refreshed.expires_at_unix, unix_secs(at(100)) + 300);
        assert!(refreshed.takeover.is_none());
    }

    #[tokio::test]
    async fn a_heartbeat_extends_the_lease_and_prevents_takeover() {
        let (_store, lease) = service();
        let workspace = Uuid::new_v4();
        let grant = lease
            .acquire(workspace, REPO, "service:agent-a", t0())
            .await
            .unwrap();
        let renewed = lease.heartbeat(&grant, at(200)).await.unwrap();
        assert_eq!(renewed.expires_at_unix, unix_secs(at(200)) + 300);
        // A would have expired at 300 without the heartbeat; with it, B still cannot take over.
        let refused = lease
            .acquire(workspace, REPO, "service:agent-b", at(400))
            .await;
        assert!(matches!(refused, Err(LeaseError::Conflict(_))));
    }

    #[tokio::test]
    async fn a_foreign_heartbeat_reports_the_current_owner() {
        let (_store, lease) = service();
        let workspace = Uuid::new_v4();
        let grant = lease
            .acquire(workspace, REPO, "service:agent-a", t0())
            .await
            .unwrap();
        let forged = LeaseGrant {
            owner: "service:agent-b".to_owned(),
            ..grant.clone()
        };
        match lease.heartbeat(&forged, at(1)).await {
            Err(LeaseError::Conflict(conflict)) => {
                assert_eq!(conflict.owner, "service:agent-a");
                assert_eq!(conflict.lease_id, grant.lease_id);
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_destroyed_workspace_records_a_release_without_a_lease_row() {
        let (store, lease) = service();
        let workspace = Uuid::new_v4();
        let grant = lease
            .acquire(workspace, REPO, "service:agent-a", t0())
            .await
            .unwrap();
        // destroy: the workspace row (and its lease row) are deleted first, then the release is
        // audited, so the evidence outlives the workspace.
        store
            .release(ReleaseRequest {
                workspace_id: workspace,
                repo: REPO,
                owner: &grant.owner,
                lease_id: grant.lease_id,
                now_unix: unix_secs(at(1)),
            })
            .await
            .unwrap();
        lease.record_released(&grant, at(2)).await.unwrap();
        let audit = store.audit(workspace);
        assert_eq!(audit.last().unwrap().event, LeaseEventKind::Released);
        assert_eq!(audit.last().unwrap().detail["reason"], "destroy");
        assert!(store.current(workspace).is_none());
    }

    #[tokio::test]
    async fn a_lease_conflict_maps_to_an_aborted_status_with_the_owner() {
        let (_store, lease) = service();
        let workspace = Uuid::new_v4();
        let grant = lease
            .acquire(workspace, REPO, "service:agent-a", t0())
            .await
            .unwrap();
        let error = lease
            .acquire(workspace, REPO, "service:agent-b", at(1))
            .await
            .unwrap_err();
        let status = error.into_status();
        assert_eq!(status.code(), Code::Aborted);
        assert!(is_lease_conflict(&status));
        let metadata = status.metadata();
        assert_eq!(
            metadata.get(METADATA_OWNER).unwrap().to_str().unwrap(),
            "service:agent-a"
        );
        assert_eq!(
            metadata.get(METADATA_LEASE_ID).unwrap().to_str().unwrap(),
            grant.lease_id.to_string()
        );
        assert_eq!(
            metadata
                .get(METADATA_WORKSPACE_ID)
                .unwrap()
                .to_str()
                .unwrap(),
            workspace.to_string()
        );
        assert_eq!(
            metadata.get(METADATA_EXPIRES_AT).unwrap().to_str().unwrap(),
            grant.expires_at_unix.to_string()
        );
    }

    #[tokio::test]
    async fn a_missing_lease_is_not_found_on_heartbeat() {
        let (_store, lease) = service();
        let workspace = Uuid::new_v4();
        let grant = LeaseGrant {
            workspace_id: workspace,
            repo: REPO.to_owned(),
            owner: "service:agent-a".to_owned(),
            lease_id: Uuid::new_v4(),
            acquired_at_unix: unix_secs(t0()),
            heartbeat_at_unix: unix_secs(t0()),
            expires_at_unix: unix_secs(t0()) + 300,
            takeover: None,
        };
        match lease.heartbeat(&grant, at(1)).await {
            Err(LeaseError::NotFound(_)) => {}
            other => panic!("expected not-found, got {other:?}"),
        }
    }

    #[test]
    fn ttl_and_unix_helpers_are_saturating() {
        assert_eq!(unix_secs(UNIX_EPOCH), 0);
        assert_eq!(unix_secs(t0()), 1_700_000_000);
        let lease = WorkspaceLease::new(Arc::new(MemoryLeaseStore::new()), Duration::ZERO);
        assert_eq!(lease.ttl(), Duration::from_secs(1), "ttl is floored at 1s");
    }

    /// A real Postgres run of the same contract. Opt-in (`FORGEVCS_TEST_DATABASE_URL`) so the
    /// normal suite needs no database; the R4 verification runs it against a throwaway local
    /// cluster, never against a shared/production branch.
    #[tokio::test]
    async fn postgres_store_enforces_the_same_contract() {
        let url = match std::env::var("FORGEVCS_TEST_DATABASE_URL") {
            Ok(url) => url,
            Err(_) => {
                eprintln!(
                    "skipping postgres lease contract: FORGEVCS_TEST_DATABASE_URL is not set"
                );
                return;
            }
        };
        let pool = test_pool(&url);
        crate::workspace::ensure_schema(&pool).await.unwrap();
        ensure_schema(&pool).await.unwrap();
        let store = Arc::new(PgLeaseStore::new(pool.clone()));
        let lease = WorkspaceLease::new(store.clone(), TTL);
        let workspace = Uuid::new_v4();
        let repo = format!("lease-test-{workspace}");
        let client = pool.get().await.unwrap();
        client
            .execute(
                "INSERT INTO forgevcs_workspaces (id, repo, state) VALUES ($1, $2, '{}'::jsonb)",
                &[&workspace, &repo],
            )
            .await
            .unwrap();

        // Free -> acquired.
        let first = lease
            .acquire(workspace, &repo, "service:agent-a", t0())
            .await
            .unwrap();
        assert_eq!(first.owner, "service:agent-a");
        // A concurrent writer at the same instant conflicts and names the owner.
        let conflict = lease
            .acquire(workspace, &repo, "service:agent-b", t0())
            .await
            .unwrap_err();
        match conflict {
            LeaseError::Conflict(conflict) => {
                assert_eq!(conflict.owner, "service:agent-a");
                assert_eq!(conflict.lease_id, first.lease_id);
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        // Stale -> takeover with an audit row.
        let second = lease
            .acquire(workspace, &repo, "service:agent-b", at(301))
            .await
            .unwrap();
        let takeover = second.takeover.clone().expect("takeover evidence");
        assert_eq!(takeover.prior_owner, "service:agent-a");
        // Release -> the next writer acquires without a takeover.
        assert!(lease.release(&second, at(302)).await.unwrap());
        let third = lease
            .acquire(workspace, &repo, "service:agent-c", at(303))
            .await
            .unwrap();
        assert!(third.takeover.is_none());

        let events = client
            .query(
                "SELECT event, owner_id, prior_owner_id FROM forgevcs_workspace_lease_events \
                 WHERE workspace_id = $1 ORDER BY at, event",
                &[&workspace],
            )
            .await
            .unwrap();
        let takeover_row = events
            .iter()
            .find(|row| row.get::<_, String>("event") == "takeover")
            .expect("takeover audit row");
        assert_eq!(takeover_row.get::<_, String>("owner_id"), "service:agent-b");
        assert_eq!(
            takeover_row.get::<_, Option<String>>("prior_owner_id"),
            Some("service:agent-a".to_owned())
        );
        assert!(
            events
                .iter()
                .any(|row| row.get::<_, String>("event") == "released")
        );
        // The free acquisition and the post-release acquisition are audited; the same-owner
        // refresh in the memory store is not.
        let acquired_owners: Vec<String> = events
            .iter()
            .filter(|row| row.get::<_, String>("event") == "acquired")
            .map(|row| row.get::<_, String>("owner_id"))
            .collect();
        assert_eq!(
            acquired_owners,
            vec!["service:agent-a".to_owned(), "service:agent-c".to_owned()]
        );

        // Clean up this test's rows (the local cluster is throwaway, but keep shared runs tidy).
        client
            .execute(
                "DELETE FROM forgevcs_workspace_lease_events WHERE workspace_id = $1",
                &[&workspace],
            )
            .await
            .unwrap();
        client
            .execute(
                "DELETE FROM forgevcs_workspaces WHERE id = $1",
                &[&workspace],
            )
            .await
            .unwrap();
    }

    /// A plaintext pool for the throwaway local cluster; the deployed path always uses TLS
    /// (`crate::db::build_pool`), which a local cluster would reject.
    fn test_pool(url: &str) -> Pool {
        use deadpool_postgres::{Manager, ManagerConfig, RecyclingMethod, Runtime};
        let config: tokio_postgres::Config = url.parse().unwrap();
        let manager = Manager::from_config(
            config,
            tokio_postgres::NoTls,
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        Pool::builder(manager)
            .max_size(2)
            .runtime(Runtime::Tokio1)
            .build()
            .unwrap()
    }
}
