//! ForgeVCS-owned Prometheus metrics, registered into the process-wide Scion registry.
//!
//! There is exactly **one** registry per process: `scion_obs::Metrics::new()` is created in
//! `main`, one `Arc` is injected into every engine (so upstream's land/CAS/RPC metrics are
//! recorded) and the same `Arc` is served by `/metrics`. This module only adds the signals Scion
//! does not own — the paused-conflict workflow — into that same registry, so one scrape answers
//! both "is land slow/failing?" and "are agents conflicting?".
//!
//! # Metric names
//!
//! Reused upstream (`scion-obs`, now populated because the registry is shared):
//! * `scion_land_attempts{result}` — `committed|idempotent|cas_mismatch|landed_conflict|failed`
//!   (the land's own retry loop is represented by multiple attempts; `cas_mismatch` is a retried
//!   ref CAS loss, `landed_conflict` an internal rebase conflict being retried).
//! * `scion_land_duration_seconds` — total land latency for terminal outcomes (all attempts).
//! * `scion_ref_cas_failures_total{ref}` — ref compare-and-swap losses (a retry, not a lost ref).
//! * `scion_rpc_duration_seconds{rpc,tier,code}`, `scion_upstream_requests_total{tier}`, …
//!
//! Added by ForgeVCS (documented in `docs/OBSERVABILITY.md`):
//! * `forgevcs_conflict_pauses_total{operation}` — `push|land|merge-land|merge|rebase|other`
//! * `forgevcs_conflict_resolves_total` — resolves that persisted a resolved changeset
//! * `forgevcs_conflict_continues_total{result}` — `ok|stale|land_failed|invalid|error|other`
//!
//! Every label is a closed set normalised by the `record_*` methods: an unknown value becomes
//! `other`, so a future caller cannot make the scrape cardinality unbounded.
//!
//! # The storage-snapshot registration lifecycle (Lane X P0 fix)
//!
//! Upstream `ServerApp::build` registers two storage snapshot collectors
//! (`scion_storage_access`, `scion_storage_bytes`) **per build** into the `Metrics` it is given
//! (`upstream/crates/scion-server/src/app.rs:251-283`). With one shared registry, the second
//! engine in a process failed to boot with `Duplicate metrics collector registration attempted`.
//!
//! [`MetricsHub`] fixes the lifecycle without disconnecting the registries:
//!
//! * the **rendered** registry (one per process) is owned by the hub; ForgeVCS registers the two
//!   storage snapshots into it **exactly once**, aggregating every live engine's
//!   `scion_store::Counters` (one process-wide series set, no per-repo label explosion);
//! * every engine receives a [`MetricsHub::engine_view`] — a `Metrics` whose built-in series
//!   handles are clones of the shared ones (so RPC/land/CAS/Cache recordings still land in the
//!   rendered registry) but whose `registry` is a private scratch registry. Upstream's per-build
//!   snapshot registration creates those collectors there and can never collide across engines;
//!   the scratch registry is not rendered (the hub's aggregate is).

use std::sync::{Arc, RwLock};

use prometheus::{IntCounter, IntCounterVec, Opts, Registry};
use scion_obs::Metrics;
use scion_store::{CounterSnapshot, Counters};

/// Metric name: paused conflicts, by operation that produced them.
pub const CONFLICT_PAUSES: &str = "forgevcs_conflict_pauses_total";
/// Metric name: paused conflicts resolved with a resolved changeset.
pub const CONFLICT_RESOLVES: &str = "forgevcs_conflict_resolves_total";
/// Metric name: continue attempts on a resolved paused conflict, by result.
pub const CONFLICT_CONTINUES: &str = "forgevcs_conflict_continues_total";

/// The `operation` label values that may appear on [`CONFLICT_PAUSES`].
pub const PAUSE_OPERATIONS: [&str; 6] = ["push", "land", "merge-land", "merge", "rebase", "other"];
/// The `result` label values that may appear on [`CONFLICT_CONTINUES`].
pub const CONTINUE_RESULTS: [&str; 6] = ["ok", "stale", "land_failed", "invalid", "error", "other"];

/// ForgeVCS's slice of the shared registry.
#[derive(Debug, Clone)]
pub struct ForgeMetrics {
    conflict_pauses: IntCounterVec,
    conflict_resolves: IntCounter,
    conflict_continues: IntCounterVec,
}

impl ForgeMetrics {
    /// Register the ForgeVCS metrics in `metrics.registry` (the process-wide registry).
    ///
    /// Returns an error when a name is already registered: two services in one process must share
    /// one [`Metrics`] handle, not register a second copy of the same series.
    pub fn register(metrics: &Metrics) -> prometheus::Result<Self> {
        let conflict_pauses = IntCounterVec::new(
            Opts::new(
                CONFLICT_PAUSES,
                "Paused merge/land conflicts by the operation that produced them.",
            ),
            &["operation"],
        )?;
        metrics
            .registry
            .register(Box::new(conflict_pauses.clone()))?;
        let conflict_resolves = IntCounter::with_opts(Opts::new(
            CONFLICT_RESOLVES,
            "Paused conflicts resolved with a resolved changeset.",
        ))?;
        metrics
            .registry
            .register(Box::new(conflict_resolves.clone()))?;
        let conflict_continues = IntCounterVec::new(
            Opts::new(
                CONFLICT_CONTINUES,
                "Continue attempts on a resolved paused conflict, by result.",
            ),
            &["result"],
        )?;
        metrics
            .registry
            .register(Box::new(conflict_continues.clone()))?;
        Ok(Self {
            conflict_pauses,
            conflict_resolves,
            conflict_continues,
        })
    }

    /// Record a paused conflict for `operation` (`push`, `land`, `merge-land`, `merge`, `rebase`).
    pub fn record_conflict_pause(&self, operation: &str) {
        self.conflict_pauses
            .with_label_values(&[normalise(operation, &PAUSE_OPERATIONS)])
            .inc();
    }

    /// Record a conflict resolve that persisted a resolved changeset.
    pub fn record_conflict_resolved(&self) {
        self.conflict_resolves.inc();
    }

    /// Record a `continue` attempt outcome (`ok`, `stale`, `land_failed`, `invalid`, `error`).
    pub fn record_conflict_continue(&self, result: &str) {
        self.conflict_continues
            .with_label_values(&[normalise(result, &CONTINUE_RESULTS)])
            .inc();
    }
}

/// Keep label cardinality closed: only the documented values are ever used.
fn normalise<'a>(value: &'a str, allowed: &[&'a str]) -> &'a str {
    allowed
        .iter()
        .copied()
        .find(|allowed| *allowed == value)
        .unwrap_or("other")
}

/// Process-wide metrics wiring: the one rendered registry, plus the once-per-process storage
/// snapshot collectors and the per-engine views that avoid duplicate registration.
pub struct MetricsHub {
    shared: Arc<Metrics>,
    storage: Arc<StorageSnapshot>,
}

impl MetricsHub {
    /// Create the process registry and register the storage snapshot collectors into it once.
    ///
    /// This is the only place the two `scion_storage_*` collectors are registered against the
    /// rendered registry; engine builds register their own copies into private scratch registries
    /// (see [`MetricsHub::engine_view`]), so booting a second repository cannot collide.
    pub fn new() -> prometheus::Result<Self> {
        let shared = Metrics::new();
        let storage = Arc::new(StorageSnapshot::default());
        storage.register(&shared)?;
        Ok(Self { shared, storage })
    }

    /// The one registry `/metrics` renders, shared by every engine's built-in series.
    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.shared
    }

    /// The `Metrics` handle to pass to one upstream `ServerApp::build`.
    ///
    /// Built-in metric handles are clones of the shared series (recordings are visible in
    /// `/metrics`); `registry` is private to this engine because upstream registers its storage
    /// snapshots into it on every build. The private registry is never rendered — the hub's
    /// aggregate storage series is.
    pub fn engine_view(&self) -> Arc<Metrics> {
        Arc::new(engine_view(&self.shared))
    }

    /// Add (or replace) one engine's storage counters in the process-wide snapshot aggregate.
    pub fn attach_storage(&self, repo: &str, counters: Arc<Counters>) {
        self.storage.attach(repo, counters);
    }

    /// Drop a destroyed engine's counters from the aggregate (repository delete / engine drop).
    pub fn detach_storage(&self, repo: &str) {
        self.storage.detach(repo);
    }

    /// How many engines currently feed the storage aggregate (diagnostics / live tests).
    pub fn storage_engines(&self) -> usize {
        self.storage.engines()
    }
}

/// Build the per-engine `Metrics` view. Field-for-field clone of the shared series handles; if a
/// vendored baseline adds a `Metrics` field, this construction fails to compile and forces the
/// view to be updated (deliberate: a silently missing handle would disconnect a signal).
fn engine_view(shared: &Metrics) -> Metrics {
    Metrics {
        // Private scratch registry: upstream's per-build `register_snapshot` calls land here.
        registry: Registry::new(),
        rpc_duration: shared.rpc_duration.clone(),
        cache_requests: shared.cache_requests.clone(),
        cache_bytes: shared.cache_bytes.clone(),
        land_attempts: shared.land_attempts.clone(),
        ref_cas_failures: shared.ref_cas_failures.clone(),
        land_duration: shared.land_duration.clone(),
        derive_duration: shared.derive_duration.clone(),
        derive_backlog: shared.derive_backlog.clone(),
        redaction_epoch: shared.redaction_epoch.clone(),
        redaction_lag: shared.redaction_lag.clone(),
        policy_epoch_info: shared.policy_epoch_info.clone(),
        object_bytes_served: shared.object_bytes_served.clone(),
        upstream_requests: shared.upstream_requests.clone(),
        integrity_failures: shared.integrity_failures.clone(),
    }
}

const STORAGE_ACCESS_HELP: &str = "Current instrumented storage access counters (logical metadata calls and rows, object storage requests).";
const STORAGE_BYTES_HELP: &str = "Current storage bytes read, before and after decompression.";

/// Aggregate `scion_storage_access` / `scion_storage_bytes` across every live engine.
///
/// One process-wide series set: values are sums over the attached engines, matching the upstream
/// names/help/labels so a scrape looks the same as the single-engine service. Per-repo series
/// would multiply cardinality by repo count for little operational value; per-repo storage use is
/// a Neon/S3-billing question, not a service-health one.
#[derive(Default)]
pub struct StorageSnapshot {
    /// `(repo, counters)` for every live engine, keyed by repo so delete/re-init replaces rows.
    engines: RwLock<Vec<(String, Arc<Counters>)>>,
}

impl StorageSnapshot {
    fn register(self: &Arc<Self>, metrics: &Metrics) -> prometheus::Result<()> {
        let access = Arc::clone(self);
        metrics.register_snapshot("scion_storage_access", STORAGE_ACCESS_HELP, move || {
            access.access_series()
        })?;
        let bytes = Arc::clone(self);
        metrics.register_snapshot("scion_storage_bytes", STORAGE_BYTES_HELP, move || {
            bytes.bytes_series()
        })
    }

    fn attach(&self, repo: &str, counters: Arc<Counters>) {
        let mut engines = self
            .engines
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(slot) = engines.iter_mut().find(|(existing, _)| existing == repo) {
            slot.1 = counters;
        } else {
            engines.push((repo.to_owned(), counters));
        }
    }

    fn detach(&self, repo: &str) {
        self.engines
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|(existing, _)| existing != repo);
    }

    /// Sum every live engine's counters into one snapshot.
    fn aggregate(&self) -> CounterSnapshot {
        let snapshots: Vec<CounterSnapshot> = self
            .engines
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(_, counters)| counters.snapshot())
            .collect();
        let mut total = CounterSnapshot::default();
        for snapshot in snapshots {
            total.meta_txns = total.meta_txns.saturating_add(snapshot.meta_txns);
            total.meta_rows_read = total.meta_rows_read.saturating_add(snapshot.meta_rows_read);
            total.object_paths_rows_read = total
                .object_paths_rows_read
                .saturating_add(snapshot.object_paths_rows_read);
            total.lease_calls = total.lease_calls.saturating_add(snapshot.lease_calls);
            total.blob_gets = total.blob_gets.saturating_add(snapshot.blob_gets);
            total.blob_puts = total.blob_puts.saturating_add(snapshot.blob_puts);
            total.blob_bytes_read = total
                .blob_bytes_read
                .saturating_add(snapshot.blob_bytes_read);
            total.object_bytes_read = total
                .object_bytes_read
                .saturating_add(snapshot.object_bytes_read);
            total.trees_written = total.trees_written.saturating_add(snapshot.trees_written);
            total.objects_written = total
                .objects_written
                .saturating_add(snapshot.objects_written);
            total.integrity_failures = total
                .integrity_failures
                .saturating_add(snapshot.integrity_failures);
        }
        total
    }

    fn access_series(&self) -> Vec<(&'static str, u64)> {
        let snapshot = self.aggregate();
        vec![
            ("metadata_calls", snapshot.meta_txns),
            ("metadata_rows", snapshot.meta_rows_read),
            ("object_path_rows", snapshot.object_paths_rows_read),
            ("object_gets", snapshot.blob_gets),
            ("object_puts", snapshot.blob_puts),
        ]
    }

    fn bytes_series(&self) -> Vec<(&'static str, u64)> {
        let snapshot = self.aggregate();
        vec![
            ("stored", snapshot.blob_bytes_read),
            ("decoded", snapshot.object_bytes_read),
        ]
    }

    /// How many engines feed the aggregate (tests / diagnostics).
    pub fn engines(&self) -> usize {
        self.engines
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn registering_into_a_fresh_registry_exports_the_forgevcs_names() {
        let metrics = Metrics::new();
        let forge = ForgeMetrics::register(&metrics).unwrap();
        forge.record_conflict_pause("land");
        forge.record_conflict_resolved();
        forge.record_conflict_continue("ok");
        // Record an upstream land series through the same handle: ForgeVCS metrics and Scion
        // metrics live in one registry, which is the point of the shared handle.
        metrics
            .land_attempts
            .with_label_values(&["committed"])
            .inc();
        let rendered = metrics.render();
        assert!(
            rendered.contains("forgevcs_conflict_pauses_total{operation=\"land\"} 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains("forgevcs_conflict_resolves_total 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains("forgevcs_conflict_continues_total{result=\"ok\"} 1"),
            "{rendered}"
        );
        // The upstream series are registered in the same registry.
        assert!(
            rendered.contains("scion_land_attempts{result=\"committed\"} 1"),
            "{rendered}"
        );
    }

    #[test]
    fn a_second_registration_is_an_error_not_a_duplicate_series() {
        let metrics = Metrics::new();
        let _first = ForgeMetrics::register(&metrics).unwrap();
        assert!(
            ForgeMetrics::register(&metrics).is_err(),
            "the same metric may not be registered twice in one registry"
        );
    }

    #[test]
    fn label_values_are_normalised_to_a_closed_set() {
        let metrics = Metrics::new();
        let forge = ForgeMetrics::register(&metrics).unwrap();
        forge.record_conflict_pause("not-a-real-operation");
        forge.record_conflict_continue("not-a-real-result");
        let rendered = metrics.render();
        assert!(
            rendered.contains("forgevcs_conflict_pauses_total{operation=\"other\"} 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains("forgevcs_conflict_continues_total{result=\"other\"} 1"),
            "{rendered}"
        );
        for value in ["not-a-real-operation", "not-a-real-result"] {
            assert!(
                !rendered.contains(value),
                "unknown label value {value:?} leaked into the scrape"
            );
        }
    }

    /// The pause call sites pass Scion/façade operation ids; `merge-land` (hyphen) is one of
    /// them and must not be normalised to `other`.
    #[test]
    fn every_pause_operation_id_is_a_first_class_label() {
        let metrics = Metrics::new();
        let forge = ForgeMetrics::register(&metrics).unwrap();
        for operation in ["push", "land", "merge-land", "merge", "rebase"] {
            forge.record_conflict_pause(operation);
        }
        let rendered = metrics.render();
        for operation in ["push", "land", "merge-land", "merge", "rebase"] {
            assert!(
                rendered.contains(&format!(
                    "forgevcs_conflict_pauses_total{{operation=\"{operation}\"}} 1"
                )),
                "missing operation {operation:?} in {rendered}"
            );
        }
    }

    /// Lane X regression (Lane V `init-defect`): upstream's per-build storage snapshot
    /// registration must not collide when a process boots a second engine.
    ///
    /// This reproduces the failure on the shared registry first (the old wiring passed that
    /// same registry to every `ServerApp::build`), then proves every engine view can perform the
    /// registration upstream performs and that their built-in series still land in `/metrics`.
    #[test]
    fn engine_views_register_storage_snapshots_without_colliding() {
        let hub = MetricsHub::new().unwrap();
        // The old defect, deterministically: the rendered registry already owns the names, so a
        // second upstream-style registration on it is a duplicate.
        assert!(
            hub.metrics()
                .register_snapshot("scion_storage_access", "duplicate", Vec::new)
                .is_err(),
            "the rendered registry must own exactly one scion_storage_access collector"
        );

        // Each engine gets a private registration registry, so upstream's per-build calls are
        // independent — this is the boot path that previously failed with
        // `Duplicate metrics collector registration attempted`.
        for _ in 0..2 {
            let view = hub.engine_view();
            view.register_snapshot("scion_storage_access", "per-build", Vec::new)
                .expect("engine view accepts its own storage collector");
            view.register_snapshot("scion_storage_bytes", "per-build", Vec::new)
                .expect("engine view accepts its own storage collector");
        }

        // The built-in handles are shared: recordings through either view are in the one scrape.
        hub.engine_view()
            .land_attempts
            .with_label_values(&["committed"])
            .inc();
        hub.engine_view()
            .land_attempts
            .with_label_values(&["failed"])
            .inc();
        let rendered = hub.metrics().render();
        assert!(
            rendered.contains("scion_land_attempts{result=\"committed\"} 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains("scion_land_attempts{result=\"failed\"} 1"),
            "{rendered}"
        );
        // One aggregated storage family, not one per engine and not zero.
        assert_eq!(
            rendered.matches("# HELP scion_storage_access ").count(),
            1,
            "{rendered}"
        );
        assert!(
            rendered.contains("# HELP scion_storage_bytes "),
            "{rendered}"
        );
    }

    /// The single storage series sums every live engine and forgets a detached one.
    #[test]
    fn storage_snapshot_aggregates_live_engines_and_detaches() {
        let hub = MetricsHub::new().unwrap();
        let repo_a = Arc::new(Counters::default());
        repo_a.record_object_read(scion_core::Kind::Blob, 128);
        let repo_b = Arc::new(Counters::default());
        repo_b.record_object_read(scion_core::Kind::Blob, 64);
        hub.attach_storage("repo-a", Arc::clone(&repo_a));
        hub.attach_storage("repo-b", Arc::clone(&repo_b));
        assert_eq!(hub.storage.engines(), 2);
        let rendered = hub.metrics().render();
        assert!(
            rendered.contains("scion_storage_bytes{kind=\"decoded\"} 192"),
            "{rendered}"
        );

        hub.detach_storage("repo-a");
        assert_eq!(hub.storage.engines(), 1);
        let rendered = hub.metrics().render();
        assert!(
            rendered.contains("scion_storage_bytes{kind=\"decoded\"} 64"),
            "{rendered}"
        );

        // Re-attaching the same repo replaces its counters instead of double counting.
        hub.attach_storage("repo-b", Arc::new(Counters::default()));
        assert_eq!(hub.storage.engines(), 1);
        let rendered = hub.metrics().render();
        assert!(
            rendered.contains("scion_storage_bytes{kind=\"decoded\"} 0"),
            "{rendered}"
        );
    }
}
