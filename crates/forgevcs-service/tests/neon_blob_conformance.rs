//! W3 acceptance against Neon Object Storage (`docs/W3-object-storage.md` §6).
//!
//! The store is built from [`Config`] — the same environment parsing the deployed service
//! uses — so this proves *our* configuration code rather than a copy of it in a test.
//!
//! Gated on the object-storage variables: when they are unset every test prints `SKIP:` and
//! passes, so `cargo test` stays hermetic and the vendored gate needs no credentials. Each
//! case gets a fresh key prefix, so repeated or concurrent runs cannot see each other's
//! objects — which is also the "fresh empty store per case" precondition of the suites.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test crate

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use forgevcs_service::config::Config;
use scion_core::ObjectId;
use scion_store::{Area, BlobStore, RepoId, conformance, object_key};
use scion_store_s3::{S3BlobStore, S3Config};

/// `Some(config)` when the object-storage variables are present; otherwise a printed skip.
fn gate(test: &str) -> Option<S3Config> {
    let config = Config::from_env();
    match config.s3_config() {
        Some(s3) if config.validate().is_ok() => Some(s3),
        _ => {
            println!("SKIP: object-storage variables unset ({test})");
            None
        }
    }
}

/// The configured store with a fresh, unique key prefix: one per suite case, every run.
fn fresh(config: &S3Config) -> S3Config {
    static CASES: AtomicU64 = AtomicU64::new(0);
    let case = CASES.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let mut config = config.clone();
    let base = config.key_prefix.trim_matches('/');
    let run = format!("w3-{stamp}-{case}");
    config.key_prefix = if base.is_empty() {
        run
    } else {
        format!("{base}/{run}")
    };
    config
}

fn client(config: S3Config) -> Arc<dyn BlobStore> {
    Arc::new(S3BlobStore::new(config).expect("S3 client")) as Arc<dyn BlobStore>
}

/// A fresh store for one suite case.
fn store(config: &S3Config) -> Arc<dyn BlobStore> {
    client(fresh(config))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn neon_blob_store_suite() {
    let Some(config) = gate("neon_blob_store_suite") else {
        return;
    };
    conformance::blob_store_suite(|| async { store(&config) }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn neon_blob_store_suite_extended() {
    let Some(config) = gate("neon_blob_store_suite_extended") else {
        return;
    };
    conformance::blob_store_suite_extended(|| async { store(&config) }).await;
}

/// One object end to end at the real key layout, through two independent clients: the second
/// must see the first's bytes (SPEC 3.5 strong read-after-write), and the key must carry the
/// BLAKE3 of what was stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn neon_strong_read_after_write_at_the_real_key_layout() {
    let Some(config) = gate("neon_strong_read_after_write_at_the_real_key_layout") else {
        return;
    };
    let shared = fresh(&config);
    let writer = client(shared.clone());
    let reader = client(shared);

    let payload = Bytes::from(vec![7u8; 200_000]);
    let id = ObjectId::hash_blob(&payload);
    let key = object_key("forgevcs", RepoId(1), &id, Area::Live);
    let fan = format!("forgevcs/r/1/b/{}", &id.hex()[..2]);
    assert!(
        key.ends_with(id.hex().as_str()),
        "key ends with the BLAKE3 hex"
    );

    writer.put(&key, payload.clone()).await.unwrap();
    assert_eq!(writer.get(&key).await.unwrap(), Some(payload.clone()));

    assert_eq!(
        reader.get(&key).await.unwrap(),
        Some(payload),
        "a second client sees the write"
    );
    assert!(reader.exists(&key).await.unwrap());
    assert_eq!(reader.list(&fan).await.unwrap(), vec![key.clone()]);

    reader.delete(&key).await.unwrap();
    assert_eq!(
        writer.get(&key).await.unwrap(),
        None,
        "the delete is visible to the other client"
    );
}
