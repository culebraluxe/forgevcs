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

use prometheus::{IntCounter, IntCounterVec, Opts};
use scion_obs::Metrics;

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
}
