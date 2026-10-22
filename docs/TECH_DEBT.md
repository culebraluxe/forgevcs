# ForgeVCS tech debt

Open items that are deliberate, known, and owned. Each entry states the gap, why it is acceptable
today, and what closing it would require.

## TD-1 — Rename engine residual limits (stage 2 shipped)

**Resolved 2026-10-18.** Rename awareness now covers the land/rebase path and the explicit
`Merge(a, b)` path, with exact-content matching plus line-set similarity for rename + edit in one
changeset. Proof: collaboration pilot 46/46 (including the merge-path fixture), 18 upstream
rename tests, hosted check 3/3.

**What remains, deliberately:** directory identity (renames are per file), both sides renaming the
same path to different destinations (explicit `add_add` conflict), and similarity-threshold tuning
with the engine version recorded in history. None block the lane workflow.

## TD-2 — Backup operational constraints (`psql` + hosted recycle)

**Status (2026-10-18, FVCS-05).** `psql` dependency **retained and documented**, not removed: the
metadata half of an archive is still a `psql COPY` export because the service exposes object bytes,
not a metadata export API. The failure mode is now loud instead of silent:

- every backup opens its own zip and verifies the manifest against `rows/*.copy` (row counts, blob
  count, refs) — `repo_backup.verify_archive`, run by `backup_all.py` and by the restore drill;
- the restore drill verifies **exact** row counts before blob import and the restored blob count
  against the archive, on top of refs and file bytes;
- `scripts/backup-lib.sh` preflights `psql` and reports the install hint; a missing client is a
  failed run with a failure alert, not a mid-zip stack trace.

**Remaining gap.** A server-side metadata export (no `psql`) and a deployment restart hook after a
restore. The hosted deployment must still be recycled after restoring into it so no instance keeps
a cached engine; Vercel instances recycle naturally in the current deployment.

**Closing it.** Add a store-backed export endpoint and call it from `repo_backup.py` instead of
`psql`, or have the restore path call a restart hook.

## TD-3 — Backup retention, offsite copy, restore drill, alerting

**Resolved (owner decision 2026-10-18, FVCS-05).** The **Mac-local zip is the accepted offsite
copy** for ForgeVCS; Neon keeps its own database backups. Delivered:

- **Daily zip** — `scripts/scheduled-backup.sh` (launchd `com.culebraluxe.forgevcs.backup`, daily
  02:00) archives every repository to `$FORGEVCS_BACKUP_DIR`
  (default `~/Library/Application Support/forgevcs-backups`), verifies each archive, and keeps the
  newest `BACKUP_RETENTION_DAYS` daily archives per repository (default 7).
- **Restore drill on the schedule** — `scripts/restore-drill.sh` (launchd
  `com.culebraluxe.forgevcs.restore-drill`, weekly Sunday 03:00) runs the full zip → verify → delete
  → restore drill in a throwaway repository, unmixed with real repositories, and writes JSON
  evidence to `$FORGEVCS_BACKUP_DIR/drills/`.
- **Failure alert** — a non-zero run writes a durable status file (daily backup:
  `$FORGEVCS_BACKUP_DIR/.backup-status.json`; drill: `.restore-drill-status.json`), appends to
  `.backup-alerts.log`, and posts a local macOS notification. No external service.
  `scripts/backup-alert-test.sh` proves the alert path without failing a real backup.
- **launchd PATH fixed** — the previously installed job failed with `cargo: command not found`
  because launchd does not inherit the login shell. Both plists now carry an explicit `PATH`, and
  every entrypoint hardens its own PATH so an older installed plist still works.
- **launchd-safe archive location** — the archive directory moved from `~/Documents` (TCC-scoped
  for Homebrew's `Python.app`; a launchd run blocks forever on an invisible consent prompt) to
  `~/Library/Application Support/forgevcs-backups`. `scripts/install-backup-schedule.sh` copies
  the legacy directory once, non-destructively; the old directory is left in place.
- **Watchdog** — a hung job (today's failure mode) is terminated after `BACKUP_JOB_TIMEOUT_SECONDS`
  (default 600; drill 900) by a watchdog that the TERM trap turns into a failed status + alert,
  instead of launchd silently keeping one instance alive and skipping the next day.

**Accepted residual risk (updated 2026-10-22).** The archive directory lives on the same Mac as the
operator: losing the laptop loses the locally-written copy. The principal's decision on 2026-10-22
re-scopes the offsite copy to **a local zip pushed to iCloud**, with a **Neon object-storage backup
branch as the fallback**; no offsite-upload code is required or integrated for that decision. What
remains accepted, not automated:

- the iCloud push is an operator step, and the weekly drill proves the local archive, not the iCloud
  copy; evidence is still written locally rather than attached to a release artifact;
- a Neon object-storage backup branch is the documented fallback (branch copy + restore), not an
  in-job step;
- if automated offsite is reopened by a later decision, the safe shape is: verified local zip →
  object/branch copy → fetched-and-restored proof → delete the proof object. (An offsite prototype
  was built and proven against the dev branch during Lane R and then stopped by the principal; it is
  uncommitted and unintegrated — do not wire it in without a new decision.)

The archive covers the 13 repository-scoped metadata tables plus object bytes; the service-owned
tables (`forgevcs_auth_decisions`, `forgevcs_repo_acl`) are re-created at boot and a repository's ACL
owner is re-claimed on `init` after a restore rather than restored from the zip. Wire
`.backup-status.json` into the daily digest when the digest work lands.

## Owned by DevOps, not debt — default repository is re-created on boot

Every service boot `ensure_repo`s the configured default repository (`forgevcs`). That is why a
pruned control plane is never fully empty: the next boot recreates it. This is by design and is
documented in `docs/M1-roles.md`; it belongs to the DevOps seat's operational model rather than
the engine's debt.

## TD-4 — Rate limiting / per-agent quotas deferred (2026-10-18)

**Gap.** No app-level rate limiting or per-agent quotas at the façade.

**Why deferred.** Free models are in use and there is no spend worth gating; quotas would be
ceremony without a real cost driver. The operational proxy now is wall time:
`Culebraluxe docs/agent/RUN-BUDGET.md` (flag over N=120 minutes, do not block).

**Reopen when.** Paid models return, multiple tenants share the service, or a real connection-pool
incident occurs. Closing requires token bucket per principal, 429 + Retry-After, observable quotas,
burst tests.

## TD-5 — Merge engine version not recorded in history (2026-10-22)

**Gap.** `FORGEVCS_MERGE_ENGINE` selects the rename-aware engine, but merge/rebase identity is a
function of `(a, b, merge engine version)` (`docs/M1-merge-analysis.md` §1) and nothing in the
repository records which engine produced an existing changeset. Two deployments with different
values can create different changeset ids for the same operation.

**State.** The value is now validated (`FORGEVCS_MERGE_ENGINE=default|exact-rename-v1`, invalid
fails `/ready`), reported by `/version`, included in push/land/merge-land responses, and logged on
every successful landing (`docs/MERGE-ENGINE.md`). Recording it inside generated changesets is
blocked upstream: `History::rebase` copies the source meta (`upstream/crates/scion-history/src/
rebase.rs:104-108`) and `History::merge` takes caller meta (`merge.rs:46,143`); a Meta extension
tag would need a SPEC tag definition and a vendored-engine change.

**Closing it.** Add an ignorable Meta extension tag (or a land/ref-ledger field) stamped by the
engine for every generated changeset, exposed by the façade, with tests proving a changed engine
value is visible on the resulting change/land record.

## TD-6 — Hosted metrics consumption: scrape/dashboards/alerts (2026-10-22, Lane U)

**Service half resolved in the shared worktree (2026-10-22, Lane U, not committed at report time).**
One process-wide `scion_obs::Metrics` is created in `main`, injected into every engine
(`Engine::connect(…, Arc<Metrics>)`) and served by `/metrics`, so `scion_land_attempts{result}`,
`scion_land_duration_seconds` and ref-CAS counters reflect real traffic; conflict-workflow counters
were added in `crates/forgevcs-service/src/metrics.rs` (`forgevcs_conflict_pauses_total{operation}`,
`..._resolves_total`, `..._continues_total{result}`). Names/labels: `docs/OBSERVABILITY.md`.

**Remaining gap (why this entry stays open).** Vercel does not scrape `/metrics`: there is no hosted
Prometheus scrape, dashboard or alert rule, and no distributed traces. The endpoint is now truthful
but nobody consumes it in production.

**Multi-repo registration lifecycle (2026-10-22, Lane X).** Sharing one registry briefly made a
second engine boot in the same process fail (`storage metrics: Duplicate metrics collector
registration attempted`, Lane V soak §7). Fixed in the shared worktree: `MetricsHub` registers
upstream's `scion_storage_access`/`scion_storage_bytes` snapshot collectors once per process,
aggregates every live engine, and hands each engine a `Metrics` view whose built-in series handles
are shared but whose registration registry is private. See `docs/OBSERVABILITY.md` §1a and
`docs/SOAK-2026-10-22.md` §7. Residual: `scion_storage_*` is a process-wide aggregate (no per-repo
label), and `engine_view` is constructed field-by-field against the vendored `Metrics` struct (a
baseline bump that adds a field fails the build until updated — deliberate).

**Closing it (packet P0-6 hosted half).** Scrape `/metrics` from a hosted collector, add an operator
dashboard and alert rules for readiness/error rate/land latency/conflict rate (and DB saturation),
prove alerts fire on an induced failure, and record a load call with non-zero series. Delete this
entry only after the Lane U change is committed and reviewed together with that evidence.

## TD-7 — Paused-conflict retention (2026-10-22, R2)

**Gap.** `forgevcs_paused_conflicts` has no TTL, pagination or standalone delete; rows accumulate
(open/resolved/continued) until the repository is deleted. The list endpoint returns every match for
a status.

**Why accepted today.** The workflow is new and volume is low; repository deletion (which removes
the rows in the same transaction) is the cleanup path.

**Closing it.** Add an operator prune script/window (cf. TD-4's auth-decision retention), paginate
the list, and decide whether resolved/continued rows may be deleted independently.

## TD-8 — Lease/audit-row retention (2026-10-22, R4)

**Gap.** `forgevcs_workspace_lease_events` keeps an audit row per acquire/renew-release/takeover;
there is no GC. Lease rows themselves are removed with the workspace (and on delete), but the event
history is unbounded.

**Closing it.** Retention window + prune script tied to the same housekeeping job as TD-7/TD-4.

## TD-9 — Mirror commit identity is not preserved (WIRE-3)

**Gap.** The git→ForgeVCS mirror verifies content/tree ids, not commit-to-changeset identity
(`docs/MIRRORING.md` "Known limits"): a mirrored tree does not reproduce the source commit id or
authorship/order exactly. The mirror is read-only while git remains the source.

**Closing it.** A WIRE-3 importer that maps git commit metadata (author/committer, timestamps,
parents, message) to changesets and records the mapping, or an explicit decision that content
identity is sufficient for the mirror's purpose.

## TD-10 — No ordered migration runner (2026-10-22)

**Gap.** The service applies its own tables with runtime `CREATE TABLE IF NOT EXISTS` DDL and
duplicates some of it as files under `migrations/` (`0001`–`0004`). There is no ordered ledger that
proves a multi-instance rollout applied the same set (R6 doc: "no migration runner yet").

**Closing it.** A ledger table + ordered apply step before readiness, with the runtime DDL reduced
to a bootstrap/no-op and parity tests between the files and the applied schema.

## TD-11 — Multi-repo-per-process metrics registration (2026-10-22, Lane X) — RESOLVED

**Was (P0, found by Lane V's soak on `ba3fe9a`).** A process that had booted one repository answered
`POST /repos/<new>/init` with `503 UNAVAILABLE: building the engine: UNAVAILABLE: storage metrics:
Duplicate metrics collector registration attempted`. The Lane U shared `scion_obs::Metrics` was
passed to every engine, and each upstream `ServerApp::build` registers the
`scion_storage_access`/`scion_storage_bytes` snapshot collectors into it.

**Fix (shared worktree, not committed at report time).** `MetricsHub` in
`crates/forgevcs-service/src/metrics.rs`: one rendered registry; the storage snapshots are registered
into it exactly once and aggregate all live engines (`attach_storage` on boot,
`detach_storage` on repository delete); each engine receives `engine_view()` — shared built-in
series handles plus a private scratch registration registry. Live before/after evidence in
`docs/SOAK-2026-10-22.md` §7; hermetic tests in `metrics::tests`; live gated test
`crates/forgevcs-service/tests/two_engine_boot.rs` (real PostgreSQL + S3, two engines, one storage
family, land counters from both).

**Residual.** One process-wide storage aggregate (per-repo storage use is a billing/capacity
question, not a health signal); `engine_view` is field-by-field against the vendored `Metrics`
struct, so a vendored-baseline bump adding a metric field is a compile-time prompt to update it
(deliberate); the 10-agent workload was not re-run with 10 repos in one process — the boot path and
metrics wiring are covered, soak-scale behaviour is not re-measured.
