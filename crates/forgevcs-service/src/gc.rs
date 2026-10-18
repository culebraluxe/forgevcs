//! Orphan blob garbage collection.
//!
//! Scion object keys are repository-scoped: `<prefix>/{r|q}/<repo_id>/<kind>/<fan>/<hex>`
//! (SPEC 5.1, `scion-store/src/keys.rs`). Deleting a repository removes its rows but leaves
//! those keys behind, so "orphan" has an exact definition: a key whose `<repo_id>` is no longer
//! in the `repos` table. Nothing shared is ever removed, because content-addressed blobs live
//! under the repository that stored them.
//!
//! The sweep is dry-run by default (`apply: false`); the DevOps seat must send `apply: true`.
//! Malformed keys are reported, never deleted.

use std::collections::BTreeSet;

use deadpool_postgres::Pool;
use scion_store::BlobStore;

/// What one sweep found and did.
#[derive(Debug, Default)]
pub struct GcReport {
    /// Blob keys examined.
    pub keys: usize,
    /// Repositories that still exist.
    pub live_repos: usize,
    /// Keys whose repository no longer exists.
    pub orphan_keys: usize,
    /// Keys deleted (zero when `apply` is false).
    pub deleted_keys: usize,
    /// Keys that do not match the SPEC 5.1 layout; reported, never deleted.
    pub malformed_keys: usize,
    /// First few malformed keys, so the operator can see what layout they use.
    pub malformed_sample: Vec<String>,
    /// First few orphan keys, for the operator's dry-run review.
    pub sample: Vec<String>,
    /// Whether deletion was requested.
    pub applied: bool,
}

/// The `<repo_id>` segment of a store-relative key (`r/42/b/ab/<hex>`), or `None` when the key
/// does not match the layout.
fn repo_id_of_key(key: &str) -> Option<i64> {
    // The canonical layout starts with `<r|q>/<repo_id>` once the configured store prefix is
    // stripped, but older acceptance runs wrote under their own prefixes
    // (`forgevcs-ci/w3-…/conf/r/1/…`), so find the first `<r|q>/<repo_id>` pair anywhere.
    let parts: Vec<&str> = key.split('/').collect();
    for (index, part) in parts.iter().enumerate() {
        if (*part == "r" || *part == "q")
            && let Some(next) = parts.get(index + 1)
            && let Ok(repo_id) = next.parse::<i64>()
        {
            return Some(repo_id);
        }
    }
    None
}

/// Delete every key under an explicit prefix (legacy namespaces). Dry-run unless `apply`.
pub async fn purge_prefix(
    blobs: &dyn BlobStore,
    prefix: &str,
    apply: bool,
) -> Result<GcReport, String> {
    let keys = blobs
        .list(prefix)
        .await
        .map_err(|error| format!("gc: listing object keys: {error}"))?;
    let mut report = GcReport {
        keys: keys.len(),
        orphan_keys: keys.len(),
        sample: keys.iter().take(20).cloned().collect(),
        applied: apply,
        ..GcReport::default()
    };
    if apply {
        for key in &keys {
            blobs
                .delete(key)
                .await
                .map_err(|error| format!("gc: deleting {key}: {error}"))?;
            report.deleted_keys += 1;
        }
    }
    Ok(report)
}

/// Sweep the bucket for keys of deleted repositories.
pub async fn sweep(blobs: &dyn BlobStore, pool: &Pool, apply: bool) -> Result<GcReport, String> {
    let client = pool
        .get()
        .await
        .map_err(|error| format!("gc: database: {error}"))?;
    let rows = client
        .query("SELECT repo_id FROM repos", &[])
        .await
        .map_err(|error| format!("gc: listing live repositories: {error}"))?;
    let live: BTreeSet<i64> = rows.iter().map(|row| row.get::<_, i64>(0)).collect();

    let keys = blobs
        .list("")
        .await
        .map_err(|error| format!("gc: listing object keys: {error}"))?;
    let mut report = GcReport {
        keys: keys.len(),
        live_repos: live.len(),
        applied: apply,
        ..GcReport::default()
    };
    let mut orphans = Vec::new();
    for key in keys {
        match repo_id_of_key(&key) {
            Some(repo_id) if !live.contains(&repo_id) => orphans.push(key.clone()),
            Some(_) => {}
            None => {
                report.malformed_keys += 1;
                if report.malformed_sample.len() < 20 {
                    report.malformed_sample.push(key);
                }
            }
        }
    }
    report.orphan_keys = orphans.len();
    report.sample = orphans.iter().take(20).cloned().collect();
    if apply {
        for key in &orphans {
            blobs
                .delete(key)
                .await
                .map_err(|error| format!("gc: deleting {key}: {error}"))?;
            report.deleted_keys += 1;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse_repo_id() {
        assert_eq!(repo_id_of_key("r/42/b/ab/abcd"), Some(42));
        assert_eq!(repo_id_of_key("q/7/t/00/0000"), Some(7));
        assert_eq!(repo_id_of_key("forgevcs-ci/w3-1/conf/r/1/b/aa/big"), Some(1));
        assert_eq!(repo_id_of_key("r/not-a-number/b/ab/abcd"), None);
        assert_eq!(repo_id_of_key("junk"), None);
        assert_eq!(repo_id_of_key("r"), None);
    }
}
