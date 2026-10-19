//! Paused conflict state: a conflict that refused a land/merge/rebase, kept so the caller can
//! resolve it and continue instead of replaying the operation by hand.
//!
//! Creation happens where the conflict is produced (`land`, `merge-land`, `push`'s land step,
//! `merge`, `rebase`). The record stores the operation, the target ref (when the operation
//! publishes one), the ref version observed at pause time, the four orientation changesets
//! (`head`, `base`, `ours`, `theirs`), the conflict report and the readable three-way file sides
//! — the same bundle `POST /conflict-context` derives on demand, captured once so `show` needs no
//! engine round trip.
//!
//! The workflow is:
//!
//! ```text
//! open ──resolve(changeset)──▶ resolved ──continue(ref unchanged)──▶ continued
//!                                        └─ref moved──────────────▶ refused (stale, still resolved)
//! ```
//!
//! `resolve` only accepts an `open` conflict and validates the resolved changeset's parent
//! against `resolve_parent`; `continue` only accepts a `resolved` conflict and refuses when the
//! ref moved since the pause (stale) or when the operation has no publishable ref and none is
//! supplied. Every transition is a compare-and-set, so two resolvers cannot both win.
//!
//! Storage mirrors the repository ACL and escalation tables: a ForgeVCS-owned table created at
//! boot when a database is configured, and an in-memory backend for a storage-less process
//! (tests/demo only — it does not survive restart).

use std::collections::BTreeMap;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use deadpool_postgres::Pool;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Valid `status` filters for the paused list; `all` disables the filter.
pub const STATUS_FILTERS: [&str; 4] = ["open", "resolved", "continued", "all"];

/// True when `status` is a filter the paused list accepts.
pub fn valid_status(status: &str) -> bool {
    STATUS_FILTERS.contains(&status)
}

/// A persisted paused conflict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// Server-assigned UUID.
    pub id: String,
    /// Repository name.
    pub repo: String,
    /// `land`, `push`, `merge-land`, `merge` or `rebase`.
    pub operation: String,
    /// `open`, `resolved` or `continued`.
    pub status: String,
    /// Target ref, when the paused operation publishes one.
    #[serde(rename = "ref")]
    pub ref_name: Option<String>,
    /// Ref version observed when the conflict was paused (the staleness guard).
    pub ref_version: Option<i64>,
    /// Ref target observed when the conflict was paused.
    pub ref_target: Option<String>,
    /// The changeset whose continuation is being resolved (the landed head for land/push; the
    /// merge tip for merge-land; `a`/`changeset` for merge/rebase).
    pub head: String,
    /// The parent the resolved changeset must have.
    pub resolve_parent: String,
    /// Merge base side, when one was known.
    pub base: Option<String>,
    /// Ours side (the target/`a`/`onto`).
    pub ours: String,
    /// Theirs side (`b`/the head being landed/replayed).
    pub theirs: String,
    /// The upstream `ConflictReport` JSON (paths, kinds, entries, hunks).
    pub report: Value,
    /// The readable three-way bundle (`conflicts` + `clean_entries`).
    pub sides: Value,
    /// Principal that hit the conflict.
    pub created_by: String,
    /// Unix seconds.
    pub created_at: i64,
    /// The changeset that resolves the conflict, once supplied.
    pub resolved_changeset: Option<String>,
    /// Principal that resolved it.
    pub resolved_by: Option<String>,
    /// Unix seconds.
    pub resolved_at: Option<i64>,
    /// Ref target after a successful continue.
    pub continued_target: Option<String>,
    /// Ref version after a successful continue.
    pub continued_version: Option<i64>,
    /// Unix seconds.
    pub continued_at: Option<i64>,
}

impl Record {
    /// The fully-qualified reference the paused operation targets, when it has one.
    pub fn target_ref(&self) -> Option<&str> {
        self.ref_name.as_deref()
    }
}

/// A conflict to persist. Filled in by the façade at the point the upstream handler refused.
#[derive(Debug, Clone)]
pub struct NewConflict {
    /// Repository name.
    pub repo: String,
    /// Operation name (`land`, `push`, `merge-land`, `merge`, `rebase`).
    pub operation: String,
    /// Target ref, when the operation publishes one.
    pub ref_name: Option<String>,
    /// Ref version observed at pause time.
    pub ref_version: Option<i64>,
    /// Ref target observed at pause time.
    pub ref_target: Option<String>,
    /// The changeset whose continuation is resolved.
    pub head: String,
    /// The parent the resolved changeset must have.
    pub resolve_parent: String,
    /// Merge base side.
    pub base: Option<String>,
    /// Ours side.
    pub ours: String,
    /// Theirs side.
    pub theirs: String,
    /// Conflict report JSON.
    pub report: Value,
    /// Readable three-way sides JSON.
    pub sides: Value,
    /// Principal that hit the conflict.
    pub created_by: String,
}

/// What `resolve` did.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolveOutcome {
    /// The conflict was open and is now resolved.
    Resolved(Box<Record>),
    /// No such conflict in this repository.
    NotFound,
    /// The conflict exists but is not open (already resolved or continued).
    NotOpen(Box<Record>),
}

/// What `continue` did.
#[derive(Debug, Clone, PartialEq)]
pub enum ContinueOutcome {
    /// The conflict was resolved and is now continued.
    Continued(Box<Record>),
    /// No such conflict in this repository.
    NotFound,
    /// The conflict exists but is not resolved yet.
    NotResolved(Box<Record>),
}

/// Where the paused conflicts live.
enum Backend {
    /// In-process (tests, demo, storage-less pilot). Not durable.
    Memory(RwLock<BTreeMap<String, Record>>),
    /// Neon, alongside the other ForgeVCS-owned tables.
    Postgres(Pool),
}

/// The paused-conflict registry.
pub struct PausedConflicts {
    backend: Backend,
}

impl PausedConflicts {
    /// An in-memory registry (tests and a storage-less process).
    pub fn memory() -> Self {
        Self {
            backend: Backend::Memory(RwLock::new(BTreeMap::new())),
        }
    }

    /// The production registry: PostgreSQL when a pool exists, memory otherwise.
    pub fn from_pool(pool: Option<Pool>) -> Self {
        match pool {
            Some(pool) => Self {
                backend: Backend::Postgres(pool),
            },
            None => Self::memory(),
        }
    }

    /// Create the table when a database backend is in use (no-op for memory).
    pub async fn ensure_schema(&self) -> Result<(), String> {
        let Backend::Postgres(pool) = &self.backend else {
            return Ok(());
        };
        let client = pool.get().await.map_err(|error| error.to_string())?;
        client
            .batch_execute(CREATE)
            .await
            .map_err(|error| format!("creating paused conflict table: {error}"))
    }

    /// Persist a new open conflict.
    pub async fn open(&self, conflict: NewConflict) -> Result<Record, String> {
        let record = Record {
            id: uuid::Uuid::new_v4().to_string(),
            repo: conflict.repo,
            operation: conflict.operation,
            status: "open".to_owned(),
            ref_name: conflict.ref_name,
            ref_version: conflict.ref_version,
            ref_target: conflict.ref_target,
            head: conflict.head,
            resolve_parent: conflict.resolve_parent,
            base: conflict.base,
            ours: conflict.ours,
            theirs: conflict.theirs,
            report: conflict.report,
            sides: conflict.sides,
            created_by: conflict.created_by,
            created_at: now_seconds(),
            resolved_changeset: None,
            resolved_by: None,
            resolved_at: None,
            continued_target: None,
            continued_version: None,
            continued_at: None,
        };
        match &self.backend {
            Backend::Memory(records) => {
                records
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(record.id.clone(), record.clone());
                Ok(record)
            }
            Backend::Postgres(pool) => {
                let client = pool.get().await.map_err(|error| error.to_string())?;
                let sql = format!(
                    "INSERT INTO forgevcs_paused_conflicts \
                     (id, repo, operation, ref_name, ref_version, ref_target, head, \
                      resolve_parent, base, ours, theirs, report, sides, created_by) \
                     VALUES ($1::text::uuid, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, \
                             $12::jsonb, $13::jsonb, $14) RETURNING {COLUMNS}"
                );
                let id = record.id.clone();
                let row = client
                    .query_one(
                        &sql,
                        &[
                            &id,
                            &record.repo,
                            &record.operation,
                            &record.ref_name,
                            &record.ref_version,
                            &record.ref_target,
                            &record.head,
                            &record.resolve_parent,
                            &record.base,
                            &record.ours,
                            &record.theirs,
                            &record.report,
                            &record.sides,
                            &record.created_by,
                        ],
                    )
                    .await
                    .map_err(|error| format!("inserting paused conflict: {error}"))?;
                row_to_record(&row)
            }
        }
    }

    /// Every paused conflict in `repo` with `status` (`all` for every state).
    pub async fn list(&self, repo: &str, status: &str) -> Result<Vec<Record>, String> {
        match &self.backend {
            Backend::Memory(records) => Ok(records
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .values()
                .filter(|record| record.repo == repo)
                .filter(|record| status == "all" || record.status == status)
                .cloned()
                .collect()),
            Backend::Postgres(pool) => {
                let client = pool.get().await.map_err(|error| error.to_string())?;
                let rows = if status == "all" {
                    let sql = format!(
                        "SELECT {COLUMNS} FROM forgevcs_paused_conflicts \
                         WHERE repo = $1 ORDER BY created_at ASC"
                    );
                    client
                        .query(&sql, &[&repo])
                        .await
                        .map_err(|error| format!("listing paused conflicts: {error}"))?
                } else {
                    let sql = format!(
                        "SELECT {COLUMNS} FROM forgevcs_paused_conflicts \
                         WHERE repo = $1 AND status = $2 ORDER BY created_at ASC"
                    );
                    client
                        .query(&sql, &[&repo, &status])
                        .await
                        .map_err(|error| format!("listing paused conflicts: {error}"))?
                };
                rows.iter().map(row_to_record).collect()
            }
        }
    }

    /// One paused conflict by id.
    pub async fn get(&self, repo: &str, id: &str) -> Result<Option<Record>, String> {
        match &self.backend {
            Backend::Memory(records) => Ok(records
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(id)
                .filter(|record| record.repo == repo)
                .cloned()),
            Backend::Postgres(pool) => {
                let client = pool.get().await.map_err(|error| error.to_string())?;
                let sql = format!(
                    "SELECT {COLUMNS} FROM forgevcs_paused_conflicts \
                     WHERE id = $1::text::uuid AND repo = $2"
                );
                let row = client
                    .query_opt(&sql, &[&id, &repo])
                    .await
                    .map_err(|error| format!("reading paused conflict: {error}"))?;
                row.as_ref().map(row_to_record).transpose()
            }
        }
    }

    /// Mark an `open` conflict resolved with the resolved changeset (compare-and-set).
    pub async fn resolve(
        &self,
        repo: &str,
        id: &str,
        who: &str,
        changeset: &str,
    ) -> Result<ResolveOutcome, String> {
        match &self.backend {
            Backend::Memory(records) => {
                let mut records = records
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let Some(record) = records.get_mut(id).filter(|record| record.repo == repo) else {
                    return Ok(ResolveOutcome::NotFound);
                };
                if record.status != "open" {
                    return Ok(ResolveOutcome::NotOpen(Box::new(record.clone())));
                }
                record.status = "resolved".to_owned();
                record.resolved_changeset = Some(changeset.to_owned());
                record.resolved_by = Some(who.to_owned());
                record.resolved_at = Some(now_seconds());
                Ok(ResolveOutcome::Resolved(Box::new(record.clone())))
            }
            Backend::Postgres(pool) => {
                let client = pool.get().await.map_err(|error| error.to_string())?;
                let sql = format!(
                    "UPDATE forgevcs_paused_conflicts \
                     SET status = 'resolved', resolved_changeset = $3, resolved_by = $4, \
                         resolved_at = now() \
                     WHERE id = $1::text::uuid AND repo = $2 AND status = 'open' \
                     RETURNING {COLUMNS}"
                );
                let row = client
                    .query_opt(&sql, &[&id, &repo, &changeset, &who])
                    .await
                    .map_err(|error| format!("resolving paused conflict: {error}"))?;
                match row {
                    Some(row) => Ok(ResolveOutcome::Resolved(Box::new(row_to_record(&row)?))),
                    None => self.not_open(repo, id).await,
                }
            }
        }
    }

    /// Mark a `resolved` conflict continued after a successful land (compare-and-set).
    pub async fn continue_landed(
        &self,
        repo: &str,
        id: &str,
        target: &str,
        version: u64,
    ) -> Result<ContinueOutcome, String> {
        let version = i64::try_from(version).unwrap_or(i64::MAX);
        match &self.backend {
            Backend::Memory(records) => {
                let mut records = records
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let Some(record) = records.get_mut(id).filter(|record| record.repo == repo) else {
                    return Ok(ContinueOutcome::NotFound);
                };
                if record.status != "resolved" {
                    return Ok(ContinueOutcome::NotResolved(Box::new(record.clone())));
                }
                record.status = "continued".to_owned();
                record.continued_target = Some(target.to_owned());
                record.continued_version = Some(version);
                record.continued_at = Some(now_seconds());
                Ok(ContinueOutcome::Continued(Box::new(record.clone())))
            }
            Backend::Postgres(pool) => {
                let client = pool.get().await.map_err(|error| error.to_string())?;
                let sql = format!(
                    "UPDATE forgevcs_paused_conflicts \
                     SET status = 'continued', continued_target = $3, continued_version = $4, \
                         continued_at = now() \
                     WHERE id = $1::text::uuid AND repo = $2 AND status = 'resolved' \
                     RETURNING {COLUMNS}"
                );
                let row = client
                    .query_opt(&sql, &[&id, &repo, &target, &version])
                    .await
                    .map_err(|error| format!("continuing paused conflict: {error}"))?;
                match row {
                    Some(row) => Ok(ContinueOutcome::Continued(Box::new(row_to_record(&row)?))),
                    None => self.not_resolved(repo, id).await,
                }
            }
        }
    }

    /// Distinguish "absent" from "not open" after a lost compare-and-set.
    async fn not_open(&self, repo: &str, id: &str) -> Result<ResolveOutcome, String> {
        Ok(match self.get(repo, id).await? {
            Some(record) => ResolveOutcome::NotOpen(Box::new(record)),
            None => ResolveOutcome::NotFound,
        })
    }

    /// Distinguish "absent" from "not resolved" after a lost compare-and-set.
    async fn not_resolved(&self, repo: &str, id: &str) -> Result<ContinueOutcome, String> {
        Ok(match self.get(repo, id).await? {
            Some(record) => ContinueOutcome::NotResolved(Box::new(record)),
            None => ContinueOutcome::NotFound,
        })
    }
}

/// The table, created at boot like the other ForgeVCS-owned tables.
const CREATE: &str = r#"
CREATE TABLE IF NOT EXISTS forgevcs_paused_conflicts (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    repo              TEXT NOT NULL,
    operation         TEXT NOT NULL,
    status            TEXT NOT NULL DEFAULT 'open',
    ref_name          TEXT,
    ref_version       BIGINT,
    ref_target        TEXT,
    head              TEXT NOT NULL,
    resolve_parent    TEXT NOT NULL,
    base              TEXT,
    ours              TEXT NOT NULL,
    theirs            TEXT NOT NULL,
    report            JSONB NOT NULL,
    sides             JSONB NOT NULL,
    created_by        TEXT NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_changeset TEXT,
    resolved_by       TEXT,
    resolved_at       TIMESTAMPTZ,
    continued_target  TEXT,
    continued_version BIGINT,
    continued_at      TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS forgevcs_paused_conflicts_status
    ON forgevcs_paused_conflicts (repo, status, created_at)
"#;

/// Column list shared by every query; `created_at`/`resolved_at`/`continued_at` are epoch
/// seconds so the memory and Postgres backends serialize identically.
const COLUMNS: &str = "id::text AS id, repo, operation, status, ref_name, ref_version, \
     ref_target, head, resolve_parent, base, ours, theirs, report, sides, created_by, \
     extract(epoch FROM created_at)::bigint AS created_at, resolved_changeset, resolved_by, \
     extract(epoch FROM resolved_at)::bigint AS resolved_at, continued_target, \
     continued_version, extract(epoch FROM continued_at)::bigint AS continued_at";

fn row_to_record(row: &tokio_postgres::Row) -> Result<Record, String> {
    Ok(Record {
        id: row.get("id"),
        repo: row.get("repo"),
        operation: row.get("operation"),
        status: row.get("status"),
        ref_name: row.get("ref_name"),
        ref_version: row.get("ref_version"),
        ref_target: row.get("ref_target"),
        head: row.get("head"),
        resolve_parent: row.get("resolve_parent"),
        base: row.get("base"),
        ours: row.get("ours"),
        theirs: row.get("theirs"),
        report: row.get("report"),
        sides: row.get("sides"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
        resolved_changeset: row.get("resolved_changeset"),
        resolved_by: row.get("resolved_by"),
        resolved_at: row.get("resolved_at"),
        continued_target: row.get("continued_target"),
        continued_version: row.get("continued_version"),
        continued_at: row.get("continued_at"),
    })
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

/// One paused conflict as the API returns it.
pub fn record_json(record: &Record) -> Value {
    serde_json::to_value(record).unwrap_or_else(
        |error| serde_json::json!({ "error": format!("serializing paused conflict: {error}") }),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn new_conflict(repo: &str) -> NewConflict {
        NewConflict {
            repo: repo.to_owned(),
            operation: "land".to_owned(),
            ref_name: Some("main".to_owned()),
            ref_version: Some(4),
            ref_target: Some("c1aa".to_owned()),
            head: "c1head".to_owned(),
            resolve_parent: "c1head".to_owned(),
            base: Some("c1parent".to_owned()),
            ours: "c1aa".to_owned(),
            theirs: "c1head".to_owned(),
            report: serde_json::json!({ "conflicts": [{ "path": "src/lib.rs" }] }),
            sides: serde_json::json!({ "conflicts": [], "clean_entries": [] }),
            created_by: "github:culebraluxe".to_owned(),
        }
    }

    #[tokio::test]
    async fn open_list_show_and_status_filters() {
        let store = PausedConflicts::memory();
        let opened = store.open(new_conflict("repo")).await.expect("open");
        assert_eq!(opened.status, "open");
        assert_eq!(opened.ref_name.as_deref(), Some("main"));
        assert_eq!(store.list("repo", "open").await.expect("list").len(), 1);
        assert_eq!(store.list("repo", "resolved").await.expect("list").len(), 0);
        assert_eq!(store.list("other", "all").await.expect("list").len(), 0);
        assert_eq!(
            store
                .get("repo", &opened.id)
                .await
                .expect("get")
                .map(|r| r.id),
            Some(opened.id.clone())
        );
        assert!(store.get("other", &opened.id).await.expect("get").is_none());
        assert!(valid_status("open"));
        assert!(valid_status("resolved"));
        assert!(valid_status("continued"));
        assert!(valid_status("all"));
        assert!(!valid_status("stale"));
    }

    #[tokio::test]
    async fn resolve_then_continue_is_a_one_way_compare_and_set() {
        let store = PausedConflicts::memory();
        let opened = store.open(new_conflict("repo")).await.expect("open");

        let resolved = store
            .resolve("repo", &opened.id, "github:culebraluxe", "c1resolved")
            .await
            .expect("resolve");
        let ResolveOutcome::Resolved(resolved) = resolved else {
            panic!("expected a resolved conflict");
        };
        assert_eq!(resolved.status, "resolved");
        assert_eq!(resolved.resolved_changeset.as_deref(), Some("c1resolved"));

        // A second resolve is refused: the conflict is not open any more.
        match store
            .resolve("repo", &opened.id, "github:culebraluxe", "c1again")
            .await
            .expect("resolve again")
        {
            ResolveOutcome::NotOpen(record) => assert_eq!(record.status, "resolved"),
            other => panic!("expected NotOpen, got {other:?}"),
        }

        // Continue before resolve is refused on a fresh conflict.
        let second = store.open(new_conflict("repo")).await.expect("open second");
        match store
            .continue_landed("repo", &second.id, "c1target", 5)
            .await
            .expect("continue too early")
        {
            ContinueOutcome::NotResolved(record) => assert_eq!(record.status, "open"),
            other => panic!("expected NotResolved, got {other:?}"),
        }

        let continued = store
            .continue_landed("repo", &opened.id, "c1target", 5)
            .await
            .expect("continue");
        let ContinueOutcome::Continued(continued) = continued else {
            panic!("expected a continued conflict");
        };
        assert_eq!(continued.status, "continued");
        assert_eq!(continued.continued_target.as_deref(), Some("c1target"));
        assert_eq!(continued.continued_version, Some(5));

        // Continued is terminal: no further resolve/continue.
        assert!(matches!(
            store
                .resolve("repo", &opened.id, "github:culebraluxe", "c1again")
                .await
                .expect("resolve after continue"),
            ResolveOutcome::NotOpen(_)
        ));
        assert!(matches!(
            store
                .continue_landed("repo", &opened.id, "c1target", 6)
                .await
                .expect("continue twice"),
            ContinueOutcome::NotResolved(_)
        ));
    }

    #[tokio::test]
    async fn unknown_ids_are_not_found_not_panics() {
        let store = PausedConflicts::memory();
        assert!(matches!(
            store
                .resolve("repo", "missing", "who", "c1")
                .await
                .expect("resolve"),
            ResolveOutcome::NotFound
        ));
        assert!(matches!(
            store
                .continue_landed("repo", "missing", "c1", 1)
                .await
                .expect("continue"),
            ContinueOutcome::NotFound
        ));
    }

    #[test]
    fn record_json_uses_the_wire_field_names() {
        let record = Record {
            id: "id".to_owned(),
            repo: "repo".to_owned(),
            operation: "land".to_owned(),
            status: "open".to_owned(),
            ref_name: Some("main".to_owned()),
            ref_version: Some(1),
            ref_target: Some("c1aa".to_owned()),
            head: "c1h".to_owned(),
            resolve_parent: "c1h".to_owned(),
            base: None,
            ours: "c1aa".to_owned(),
            theirs: "c1h".to_owned(),
            report: serde_json::json!({}),
            sides: serde_json::json!({}),
            created_by: "github:culebraluxe".to_owned(),
            created_at: 7,
            resolved_changeset: None,
            resolved_by: None,
            resolved_at: None,
            continued_target: None,
            continued_version: None,
            continued_at: None,
        };
        let json = record_json(&record);
        assert_eq!(json["ref"], "main");
        assert_eq!(json["operation"], "land");
        assert_eq!(json["status"], "open");
        assert_eq!(json["created_at"], 7);
    }
}
