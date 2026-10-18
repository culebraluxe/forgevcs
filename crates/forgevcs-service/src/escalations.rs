//! Conflict escalation work queue (ForgeVCS-owned table).
//!
//! A worker that cannot resolve a merge or rebase conflict escalates it here instead of
//! guessing. The DevOps seat lists, claims and resolves the items; the orchestrator can poll
//! `GET /repos/{repo}/conflicts?status=open` and launch a DevOps agent for each item.
//!
//! The queue stores pointers, not content: repository, ref, head/onto/base changeset ids, the
//! caller's note and the conflict report. The DevOps agent re-derives the three sides with
//! `POST /repos/{repo}/conflict-context` before resolving, so nothing here can drift from the
//! repository.
//!
//! Schema follows the other ForgeVCS-owned tables (`forgevcs_workspaces`,
//! `forgevcs_auth_decisions`): created at startup with `CREATE TABLE IF NOT EXISTS`, no migration
//! runner yet (W2 gap).

use deadpool_postgres::Pool;
use serde_json::Value;

/// Valid `status` filters for the list endpoint; `all` disables the filter.
pub const STATUS_FILTERS: [&str; 4] = ["open", "claimed", "resolved", "all"];

/// True when `status` is a filter the list endpoint accepts.
pub fn valid_status(status: &str) -> bool {
    STATUS_FILTERS.contains(&status)
}

const CREATE: &str = r#"
CREATE TABLE IF NOT EXISTS forgevcs_conflict_escalations (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    repo        TEXT NOT NULL,
    ref_name    TEXT NOT NULL,
    head        TEXT NOT NULL,
    onto        TEXT,
    base        TEXT,
    note        TEXT,
    report      JSONB,
    status      TEXT NOT NULL DEFAULT 'open',
    created_by  TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    claimed_by  TEXT,
    claimed_at  TIMESTAMPTZ,
    resolution  TEXT,
    resolved_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS forgevcs_conflict_escalations_status
    ON forgevcs_conflict_escalations (repo, status, created_at)
"#;

const COLUMNS: &str = "id::text AS id, repo, ref_name, head, onto, base, note, status, \
     created_by, created_at::text AS created_at, claimed_by, claimed_at::text AS claimed_at, \
     resolution, resolved_at::text AS resolved_at, report";

/// Create the queue table if it does not exist. A failure is logged by the boot path, not fatal:
/// the engine stays up and the queue endpoints answer 503.
pub async fn ensure_schema(pool: &Pool) -> Result<(), String> {
    let client = pool.get().await.map_err(|error| error.to_string())?;
    client
        .batch_execute(CREATE)
        .await
        .map_err(|error| format!("creating conflict escalation table: {error}"))
}

fn row_json(row: &tokio_postgres::Row) -> Value {
    serde_json::json!({
        "id": row.get::<_, String>("id"),
        "repo": row.get::<_, String>("repo"),
        "ref": row.get::<_, String>("ref_name"),
        "head": row.get::<_, String>("head"),
        "onto": row.get::<_, Option<String>>("onto"),
        "base": row.get::<_, Option<String>>("base"),
        "note": row.get::<_, Option<String>>("note"),
        "status": row.get::<_, String>("status"),
        "created_by": row.get::<_, String>("created_by"),
        "created_at": row.get::<_, String>("created_at"),
        "claimed_by": row.get::<_, Option<String>>("claimed_by"),
        "claimed_at": row.get::<_, Option<String>>("claimed_at"),
        "resolution": row.get::<_, Option<String>>("resolution"),
        "resolved_at": row.get::<_, Option<String>>("resolved_at"),
        "report": row.get::<_, Option<Value>>("report"),
    })
}

/// Insert one escalation and return its row.
#[allow(clippy::too_many_arguments)]
pub async fn insert(
    pool: &Pool,
    repo: &str,
    ref_name: &str,
    head: &str,
    onto: Option<&str>,
    base: Option<&str>,
    note: Option<&str>,
    report: Option<&Value>,
    created_by: &str,
) -> Result<Value, String> {
    let client = pool.get().await.map_err(|error| error.to_string())?;
    let sql = format!(
        "INSERT INTO forgevcs_conflict_escalations \
         (repo, ref_name, head, onto, base, note, report, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, $8) RETURNING {COLUMNS}"
    );
    let report = report.cloned();
    let row = client
        .query_one(
            &sql,
            &[
                &repo,
                &ref_name,
                &head,
                &onto,
                &base,
                &note,
                &report,
                &created_by,
            ],
        )
        .await
        .map_err(|error| format!("inserting escalation: {error}"))?;
    Ok(row_json(&row))
}

/// List escalations for one repository, optionally filtered by status.
pub async fn list(pool: &Pool, repo: &str, status: &str) -> Result<Vec<Value>, String> {
    let client = pool.get().await.map_err(|error| error.to_string())?;
    let rows = if status == "all" {
        let sql = format!(
            "SELECT {COLUMNS} FROM forgevcs_conflict_escalations \
             WHERE repo = $1 ORDER BY created_at ASC"
        );
        client
            .query(&sql, &[&repo])
            .await
            .map_err(|error| format!("listing escalations: {error}"))?
    } else {
        let sql = format!(
            "SELECT {COLUMNS} FROM forgevcs_conflict_escalations \
             WHERE repo = $1 AND status = $2 ORDER BY created_at ASC"
        );
        client
            .query(&sql, &[&repo, &status])
            .await
            .map_err(|error| format!("listing escalations: {error}"))?
    };
    Ok(rows.iter().map(row_json).collect())
}

/// Claim an open escalation as the DevOps seat. `None` means it was not open any more.
pub async fn claim(pool: &Pool, repo: &str, id: &str, who: &str) -> Result<Option<Value>, String> {
    let client = pool.get().await.map_err(|error| error.to_string())?;
    let sql = format!(
        "UPDATE forgevcs_conflict_escalations \
         SET status = 'claimed', claimed_by = $3, claimed_at = now() \
         WHERE id = $1::text::uuid AND repo = $2 AND status = 'open' RETURNING {COLUMNS}"
    );
    let rows = client
        .query(&sql, &[&id, &repo, &who])
        .await
        .map_err(|error| format!("claiming escalation: {error}"))?;
    Ok(rows.first().map(row_json))
}

/// Resolve an open or claimed escalation. `None` means it was already resolved (or absent).
pub async fn resolve(
    pool: &Pool,
    repo: &str,
    id: &str,
    who: &str,
    resolution: Option<&str>,
) -> Result<Option<Value>, String> {
    let client = pool.get().await.map_err(|error| error.to_string())?;
    let sql = format!(
        "UPDATE forgevcs_conflict_escalations \
         SET status = 'resolved', resolution = $4, resolved_at = now(), \
             claimed_by = COALESCE(claimed_by, $3) \
         WHERE id = $1::text::uuid AND repo = $2 AND status IN ('open', 'claimed') RETURNING {COLUMNS}"
    );
    let rows = client
        .query(&sql, &[&id, &repo, &who, &resolution])
        .await
        .map_err(|error| format!("resolving escalation: {error}"))?;
    Ok(rows.first().map(row_json))
}

/// Delete one escalation (DevOps clearing history). Returns whether a row was removed.
pub async fn delete(pool: &Pool, repo: &str, id: &str) -> Result<bool, String> {
    let client = pool.get().await.map_err(|error| error.to_string())?;
    let removed = client
        .execute(
            "DELETE FROM forgevcs_conflict_escalations \
             WHERE id = $1::text::uuid AND repo = $2",
            &[&id, &repo],
        )
        .await
        .map_err(|error| format!("deleting escalation: {error}"))?;
    Ok(removed > 0)
}

/// Delete every escalation for `repo` in `status`; `all` clears the queue's history.
pub async fn clear(pool: &Pool, repo: &str, status: &str) -> Result<u64, String> {
    let client = pool.get().await.map_err(|error| error.to_string())?;
    let removed = if status == "all" {
        client
            .execute(
                "DELETE FROM forgevcs_conflict_escalations WHERE repo = $1",
                &[&repo],
            )
            .await
    } else {
        client
            .execute(
                "DELETE FROM forgevcs_conflict_escalations WHERE repo = $1 AND status = $2",
                &[&repo, &status],
            )
            .await
    }
    .map_err(|error| format!("clearing escalations: {error}"))?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_filters_are_validated() {
        assert!(valid_status("open"));
        assert!(valid_status("claimed"));
        assert!(valid_status("resolved"));
        assert!(valid_status("all"));
        assert!(!valid_status("done"));
        assert!(!valid_status(""));
    }
}
