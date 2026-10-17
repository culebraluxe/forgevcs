//! PostgreSQL/Neon conformance run (`postgres` feature; `docs/adr/0001-infrastructure-stack.md`
//! §7.1).
//!
//! Gated on `FORGEVCS_DATABASE_URL_UNPOOLED`, falling back to `SCION_TEST_CRDB_URL`: when
//! neither is set the test prints `SKIP:` and passes.
//!
//! Unlike `crdb_external`, which creates a throwaway database per case, this run keeps one
//! database and isolates each suite case by repository name: `repo_id` is derived from the name
//! and every SPEC 5.2 table is keyed by it first, so the cases cannot see each other's rows.
//! That keeps the run inside Neon's project quotas and avoids `CREATE DATABASE` round trips.
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test crate

use scion_store::{MetaStore, conformance};
use scion_store_crdb::{CrdbConfig, CrdbMetaStore, dialect};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The unpooled URL: Neon's pooler must not be used (transaction pooling breaks the prepared
/// statements and session `SERIALIZABLE` this store depends on).
fn url() -> Option<String> {
    ["FORGEVCS_DATABASE_URL_UNPOOLED", "SCION_TEST_CRDB_URL"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|s| !s.trim().is_empty()))
}

/// Distinguishes repositories within one run.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// The full [`conformance::meta_store_suite`] against the configured PostgreSQL/Neon database.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn meta_store_suite_against_neon() {
    let Some(url) = url() else {
        println!("SKIP: FORGEVCS_DATABASE_URL_UNPOOLED unset (neon meta_store_suite)");
        return;
    };
    let tls = dialect::tls_client_config().expect("rustls client config");
    let run = std::process::id();
    conformance::meta_store_suite(move || {
        let url = url.clone();
        let tls = tls.clone();
        async move {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let store = CrdbMetaStore::connect(CrdbConfig {
                url,
                max_connections: 4,
                repo_name: format!("forgevcs_{run}_{n}"),
                tls: Some(tls),
            })
            .await
            .expect("connect to Neon");
            Arc::new(store) as Arc<dyn MetaStore>
        }
    })
    .await;
}
