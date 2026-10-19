# Auth-decision retention (R6, trimmed)

**Status:** delivered 2026-10-19. Scope: `scripts/prune-auth-decisions.{py,sh}`, this doc. No
migration runner, no service change, no deploy, no scheduled job.

## What is retained

`forgevcs_auth_decisions` holds one row per authorization decision evaluated by the W6 A/B
harness (`service`, `subject`, `action`, `repository`, `path`, `ref_name`, allow/mismatch fields,
`mode`, `reason`). It is **audit evidence**, not source of truth: losing old rows removes
debuggability, not correctness. The table is created and indexed at authorization startup:

```sql
CREATE INDEX IF NOT EXISTS forgevcs_auth_decisions_at ON forgevcs_auth_decisions (at DESC);
```

The table is service-owned, so retention needs no schema change and no migration runner — only a
window and a batched `DELETE`.

## Tool

```sh
scripts/prune-auth-decisions.sh                        # dry run, 90-day window
scripts/prune-auth-decisions.sh --days 30 --json       # dry run, machine-readable counts
scripts/prune-auth-decisions.sh --apply --confirm      # delete (explicit double opt-in)
```

- Window: `--days N` or `FORGEVCS_AUTH_DECISIONS_RETENTION_DAYS`; **default 90**, minimum 1.
- **Dry run is the default** and prints: table, window + cutoff, rows total, rows older than the
  window, rows kept, oldest row, newest row, and the mode (`DRY RUN — nothing deleted`).
- `--apply` requires `--confirm`; it deletes in batches (`--batch-size`, default 5000), then
  prints `rows_deleted` and re-reads the counts. The DELETE is idempotent and uses the `at` index,
  so it is safe while a single instance serves; no restart, no lock that blocks new decisions.
- Database target: `FORGEVCS_DATABASE_URL` / `DATABASE_URL` (the shell wrapper loads `.env.local`
  from `neon env pull`). No production-specific path is hardcoded; use the branch/env you intend.
- Requires the `psql` client (same precedent as the backup scripts, TD-2); no Python database
  driver is added.
- `--json` emits the same result as JSON for a job/alert wrapper.

## Evidence (2026-10-19, `.env.local` branch `forgevcs-ci`)

Dry run only, no rows deleted (run after the live lane-scope acceptance, which is what added the
human read/write/update-ref and `token_mint` rows at 08:12Z):

```
auth-decision retention
  table:                 forgevcs_auth_decisions
  window:                90 days (cutoff 2026-07-21 08:12:20.935317+00)
  rows total:            25
  older than window:     0
  oldest row:            2026-10-17 13:29:08.97589+00
  newest row:            2026-10-19 08:12:05.63305+00
  mode:                  DRY RUN — nothing deleted (--apply --confirm to prune)
```

`--json` returns the same counts (`mode: "dry-run"`, `rows_total`, `rows_older_than_window`,
`rows_kept`, `cutoff`, oldest/newest). Guards verified: `--days 0` and `--apply` without
`--confirm` both exit 2 without touching the database. No rows were deleted.

## Deliberately not included

- **No scheduler.** Running it daily/weekly is an operator decision (launchd/cron alongside the
  existing backup jobs), not this work order.
- **No multi-instance migration runner.** Retention here is a single batched DELETE on a
  service-owned table; there is no schema migration to coordinate, and `--apply` is safe to run
  while one instance serves.
- **No change to which decisions are recorded.** The W6 mode (`SCION_ONLY`, `SHADOW_COMPARE`,
  `DUAL_ENFORCE`) and `forgevcs_auth_decisions`' schema are unchanged.

## Operator checklist

1. Choose the window (default 90 days) and confirm the database URL/branch.
2. Run the dry run; check `rows older than window` and the oldest/newest range.
3. Run `--apply --confirm` during a quiet period; check `rows_deleted` and the re-read counts.
4. Optional: wire `--json` into the existing backup/digest job once the schedule is decided.
