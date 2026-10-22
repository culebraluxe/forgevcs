//! Lane X regression: two real engines in one process (Lane V's `init-defect`).
//!
//! Lane V's 10-agent soak found that, after the P0-metrics work shared one process-wide
//! `scion_obs::Metrics` between every engine, a process that had already booted a repository
//! answered `POST /repos/<other>/init` with
//! `503 UNAVAILABLE: storage metrics: Duplicate metrics collector registration attempted`:
//! upstream `ServerApp::build` registers `scion_storage_access`/`scion_storage_bytes` per build.
//!
//! `MetricsHub` now registers those collectors once against the rendered registry (aggregating
//! all engines) and gives each engine a view with a private registration registry. This test
//! boots **two real engines** (real PostgreSQL + real S3-compatible storage) in one process and
//! asserts the second boot succeeds and `/metrics` still carries one storage series set plus the
//! built-in land counters from both engines.
//!
//! It is opt-in: a normal `cargo test` (no database/bucket) skips it with a message. Run it
//! against the local harness used by Lane V:
//!
//! ```sh
//! FORGEVCS_TEST_DATABASE_URL=postgres://user@127.0.0.1:5432/forgevcs_test \
//! FORGEVCS_TEST_S3_ENDPOINT=https://127.0.0.1:9443 \
//! FORGEVCS_TEST_S3_BUCKET=forgevcs-test \
//! FORGEVCS_TEST_S3_ACCESS_KEY=fake FORGEVCS_TEST_S3_SECRET_KEY=fake \
//! SSL_CERT_FILE=/tmp/fvcs-verify/ca.pem \
//!   cargo test -p forgevcs-service --test two_engine_boot -- --nocapture
//! ```
//!
//! The two repositories are left in the test database (unique names per run); they are local
//! harness rows, never a shared/production database.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test crate

use std::path::Path;
use std::sync::Arc;

use forgevcs_service::config::Config;
use forgevcs_service::engine::Engine;
use forgevcs_service::metrics::MetricsHub;

/// Build a `Config` from the live-harness environment, or `None` when the harness is absent.
fn live_config() -> Option<Config> {
    let database_url = std::env::var("FORGEVCS_TEST_DATABASE_URL").ok()?;
    let endpoint = std::env::var("FORGEVCS_TEST_S3_ENDPOINT").ok()?;
    let bucket = std::env::var("FORGEVCS_TEST_S3_BUCKET").ok()?;
    let policy = std::env::var("FORGEVCS_TEST_POLICY").unwrap_or_else(|_| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../acl.toml")
            .to_string_lossy()
            .into_owned()
    });
    let region =
        std::env::var("FORGEVCS_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".to_owned());
    let access_key =
        std::env::var("FORGEVCS_TEST_S3_ACCESS_KEY").unwrap_or_else(|_| "fake-access".to_owned());
    let secret_key =
        std::env::var("FORGEVCS_TEST_S3_SECRET_KEY").unwrap_or_else(|_| "fake-secret".to_owned());
    Some(Config::from_lookup(move |key| {
        Some(match key {
            "FORGEVCS_DATABASE_URL" => database_url.clone(),
            "FORGEVCS_BUCKET" => bucket.clone(),
            "FORGEVCS_AWS_REGION" => region.clone(),
            "FORGEVCS_AWS_ENDPOINT_URL_S3" => endpoint.clone(),
            "FORGEVCS_AWS_ACCESS_KEY_ID" => access_key.clone(),
            "FORGEVCS_AWS_SECRET_ACCESS_KEY" => secret_key.clone(),
            "FORGEVCS_POLICY_PATH" => policy.clone(),
            _ => return None,
        })
    }))
}

fn unique_repo(prefix: &str) -> String {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}-{}", &suffix[..10])
}

/// Parse `scion_storage_access{kind="metadata_calls"} 42` from an exposition body.
fn storage_value(body: &str, kind: &str) -> u64 {
    let prefix = format!("scion_storage_access{{kind=\"{kind}\"}} ");
    body.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_else(|| panic!("missing {prefix} in /metrics body:\n{body}"))
}

#[tokio::test]
async fn two_real_engines_boot_in_one_process_and_share_metrics() {
    let Some(config) = live_config() else {
        eprintln!(
            "skipping two_engine_boot: set FORGEVCS_TEST_DATABASE_URL and \
             FORGEVCS_TEST_S3_ENDPOINT/BUCKET (+ SSL_CERT_FILE for the mock TLS CA) to run it"
        );
        return;
    };

    let hub = Arc::new(MetricsHub::new().expect("fresh metrics hub"));
    let repo_a = unique_repo("fvcs-x2-a");
    let repo_b = unique_repo("fvcs-x2-b");

    // Before Lane X, the second call failed with
    // `storage metrics: Duplicate metrics collector registration attempted`.
    let engine_a = Engine::connect(&config, &repo_a, Arc::clone(&hub))
        .await
        .expect("first engine boots");
    let engine_b = Engine::connect(&config, &repo_b, Arc::clone(&hub))
        .await
        .expect("second engine boots in the same process (Lane V init-defect regression)");
    assert_eq!(hub.storage_engines(), 2);

    // Built-in handles are shared: a recording through either engine appears once, in the one
    // rendered registry (`/metrics` serves exactly this body).
    engine_a
        .context()
        .metrics
        .land_attempts
        .with_label_values(&["committed"])
        .inc();
    engine_b
        .context()
        .metrics
        .land_attempts
        .with_label_values(&["failed"])
        .inc();
    let rendered = hub.metrics().render();
    assert!(
        rendered.contains("scion_land_attempts{result=\"committed\"} 1"),
        "engine A's land counter missing from the shared scrape:\n{rendered}"
    );
    assert!(
        rendered.contains("scion_land_attempts{result=\"failed\"} 1"),
        "engine B's land counter missing from the shared scrape:\n{rendered}"
    );

    // One aggregated storage series set, not one per engine and not an empty registry: booting a
    // repository itself performs metadata/object access, so the aggregate must be non-zero.
    assert_eq!(
        rendered.matches("# HELP scion_storage_access ").count(),
        1,
        "storage snapshots must be registered once for the process:\n{rendered}"
    );
    assert!(
        storage_value(&rendered, "metadata_calls") > 0,
        "the aggregate must include the engines' boot traffic:\n{rendered}"
    );

    // Destroying one engine drops its contribution.
    drop(engine_b);
    hub.detach_storage(&repo_b);
    assert_eq!(hub.storage_engines(), 1);
}
