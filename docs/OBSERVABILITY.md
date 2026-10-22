# Observability — metrics registry, land/CAS and conflict signals

Status: **service-side wired** (Lane U, 2026-10-22); **multi-repo registration lifecycle fixed**
(Lane X, 2026-10-22). Hosted scrape/dashboards/alerts remain open (§4). Companion to
`docs/M1-merge-analysis.md` gap row 6.

## 1. One registry per process (the P0 defect and its fix)

Before Lane U the service created a fresh `scion_obs::Metrics::new()` in three unrelated places:
`main`/tests never shared one, every `Engine::connect` created its own, and `router()` created yet
another for `/metrics`. The consequences were exactly what Lane Q reported: the engine incremented
a registry nobody scraped, so a hosted `/metrics` showed an **all-zero land histogram**.

Now there is **one** `Arc<Metrics>` per process, owned by a `MetricsHub`:

```
main()
  ├─ MetricsHub::new()                    // the only Metrics::new() outside tests
  │     ├─ shared registry (rendered)
  │     └─ registers scion_storage_access/bytes ONCE (aggregate over live engines)
  ├─ ForgeMetrics::register(hub.metrics())// ForgeVCS conflict counters → same registry
  ├─ Engines::boot(&config, hub.clone())
  │     └─ Engine::connect(config, repo, hub)
  │           ├─ hub.engine_view()        // shared series handles + private scratch registry
  │           └─ hub.attach_storage(repo, ctx.storage.counters)  // after build
  └─ AppState { metrics: hub.metrics(), forge_metrics }
        └─ router() → health_router(state.metrics, ready)   // /metrics renders it
```

Invariants (pinned by tests):

- `Engines::metrics()` and `AppState::metrics` are the **same `Arc`** (`http::tests::
  metrics_endpoint_reflects_engine_operations`).
- Two `Engines` built with the same hub share the registry; one built with a fresh hub does
  not (`http::tests::two_engines_share_one_metrics_registry`).
- A metric recorded through the engine handle appears in a `/metrics` scrape, including
  `scion_land_attempts{result}`, `scion_ref_cas_failures_total{ref}` and
  `scion_land_duration_seconds_count` (same test — the regression guard for the old defect).
- Duplicate registration is an error, not a second series (`metrics::tests::
  a_second_registration_is_an_error_not_a_duplicate_series`).

Rule for future code: **never call `Metrics::new()` outside `main` and tests.** Take the hub /
shared handle (engines get `MetricsHub` injected; handlers use `AppState::metrics` /
`AppState::forge_metrics`).

### 1a. Storage snapshots: once per process, not per engine (Lane X P0 fix)

Upstream `ServerApp::build` registers two storage snapshot collectors **on every build**
(`scion_storage_access`, `scion_storage_bytes`; `upstream/crates/scion-server/src/app.rs:251-283`).
After Lane U passed the one shared `Metrics` to every engine, the second engine in a process died
with `storage metrics: Duplicate metrics collector registration attempted`, and
`POST /repos/<new>/init` returned 503 (found by Lane V's soak; reproduced before/after in
`docs/SOAK-2026-10-22.md` §7).

`MetricsHub` fixes the registration lifecycle without disconnecting the registry:

- the two snapshot collectors are registered **exactly once** into the rendered registry, and their
  closures sum `scion_store::Counters` over every live engine (`MetricsHub::attach_storage` on
  boot, `detach_storage` on repository delete). Names/help/labels stay identical to upstream;
- every engine gets a `MetricsHub::engine_view()`: built-in series handles are clones of the shared
  ones, but `registry` is a **private scratch registry**. Upstream's per-build snapshot
  registration lands there and can never collide; it is not rendered (the hub's aggregate is);
- `scion_storage_*` is a **process-wide aggregate**, not per repo. Per-repo storage use is a
  billing/capacity question for Neon/S3, not a health signal, and a `repo` label would multiply
  cardinality by repository count.

Tests: `metrics::tests::engine_views_register_storage_snapshots_without_colliding` (reproduces the
duplicate on the rendered registry, then proves two engine views register independently),
`storage_snapshot_aggregates_live_engines_and_detaches`, and the live opt-in
`tests/two_engine_boot.rs::two_real_engines_boot_in_one_process_and_share_metrics` (real
PostgreSQL + S3, two engines, one storage family, land counters from both). A multi-repo-per-process
topology is supported again; process-per-repo remains valid.

## 2. Metric names

### Reused from Scion (`scion-obs`, now populated)

| Metric | Labels | Meaning |
| --- | --- | --- |
| `scion_land_attempts` | `result` ∈ `committed`, `idempotent`, `cas_mismatch`, `landed_conflict`, `failed` | one increment per land attempt; `cas_mismatch` = ref CAS lost and retried, `landed_conflict` = internal rebase conflict retried |
| `scion_land_duration_seconds` | — | total land latency for terminal outcomes (`committed`, `idempotent`, `failed`), all attempts included |
| `scion_ref_cas_failures_total` | `ref` | ref compare-and-swap losses (a retried land, not a lost ref) |
| `scion_rpc_duration_seconds` | `rpc`, `tier`, `code` | in-process handler latency/status |
| `scion_storage_access` | `kind` ∈ `metadata_calls`, `metadata_rows`, `object_path_rows`, `object_gets`, `object_puts` | instrumented storage access, **summed over every live engine** in the process (gauge; upstream registers it per build, the hub once) |
| `scion_storage_bytes` | `kind` ∈ `stored`, `decoded` | storage bytes read, summed over live engines |

Ref-CAS retries are therefore visible as `scion_land_attempts{result="cas_mismatch"}` (attempt)
plus `scion_ref_cas_failures_total{ref}` (loss). The retries themselves happen inside Scion's
land loop, so there is no separate ForgeVCS retry counter to add.

### Added by ForgeVCS (`crates/forgevcs-service/src/metrics.rs`)

| Metric | Labels | Meaning |
| --- | --- | --- |
| `forgevcs_conflict_pauses_total` | `operation` ∈ `push`, `land`, `merge-land`, `merge`, `rebase`, `other` | a merge/land conflict was persisted as a paused conflict (the "agents are colliding" signal) |
| `forgevcs_conflict_resolves_total` | — | a paused conflict was resolved with a resolved changeset |
| `forgevcs_conflict_continues_total` | `result` ∈ `ok`, `stale`, `land_failed`, `invalid`, `error`, `other` | a `continue` attempt and its outcome (`stale` = ref moved since pause; `land_failed` = the land failed again, e.g. a fresh CAS/conflict) |

Labels are a **closed set**: the `record_*` methods normalise anything else to `other`, so a
future caller cannot make scrape cardinality unbounded. Do not add per-agent, per-repo or
per-path labels; those belong in logs/audit rows, not Prometheus.

Useful queries:

```promql
# conflict pauses per operation per hour
sum by (operation) (rate(forgevcs_conflict_pauses_total[1h]))
# land outcomes
sum by (result) (rate(scion_land_attempts[5m]))
# p95 land latency from the histogram
histogram_quantile(0.95, sum(rate(scion_land_duration_seconds_bucket[5m])) by (le))
```

## 3. Verifying it

```bash
cargo test -p forgevcs-service --lib metrics      # registry + label tests
cargo test -p forgevcs-service                    # includes the /metrics wiring tests
# against a local/hosted instance (no auth is required for the upstream probes):
curl -s http://127.0.0.1:3000/metrics | grep -E '^(scion_land|forgevcs_conflict)'
```

The public router serves Scion's `health_router` unchanged at `/metrics`, `/healthz`, `/readyz`
(ADR-0001 §4); this work did not change those routes, only which registry `/metrics` reads.

## 4. What is still missing (hosted follow-up)

1. **Nothing scrapes `/metrics`.** Vercel does not provide Prometheus scraping; production needs an
   external actor (Prometheus Agent/Alloy/Vector, Grafana Cloud, or a scheduled collector) that
   pulls the deployed `/metrics` and stores the series. Until then the metrics are correct but
   unobserved.
2. **No dashboard.** Suggested first panels: land outcomes by `result`; p95/p99
   `scion_land_duration_seconds`; conflict pauses by `operation`; conflict continue outcomes;
   `scion_ref_cas_failures_total` rate (contention indicator).
3. **No alert rules.** Minimum: land failure ratio over a window; conflict-pause rate spike;
   sustained ref-CAS failures; `/ready` 503 (the probe already exists; no one alerts on it).
4. **No traces.** This work adds metrics only; request-level tracing across the façade → engine →
   store remains a separate item (merge-analysis gap row 6, §4).
5. `/metrics` is unauthenticated (upstream `health_router` behaviour). If the deployment is
   publicly reachable, restrict it at the edge rather than changing the vendored router.
6. **Metrics are per process, and counters reset when a container starts.** Vercel scales to zero
   and starts fresh instances, so with more than one live instance the scrape target has to be
   aggregated (`sum(rate(...))` across instances), and a counter is not a lifetime total. A
   scraper must tolerate target churn/`up` gaps; use `rate`/`increase` over a window, not raw
   counter values, for alerting.
