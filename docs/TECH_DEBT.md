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

**Accepted residual risk.** The archive directory lives on the same Mac as the operator: losing the
laptop loses the zip copy (Neon history is the other copy). There is still no object-storage/Neon-
branch archive copy, and the drill evidence is written locally rather than attached to a release
artifact. The archive covers the 13 repository-scoped metadata tables plus object bytes; the
service-owned tables (`forgevcs_auth_decisions`, `forgevcs_repo_acl`) are re-created at boot and a
repository's ACL owner is re-claimed on `init` after a restore rather than restored from the zip. If offsite is revisited, ship the verified zips to object storage or a Neon branch and
wire `.backup-status.json` into the daily digest.

## Owned by DevOps, not debt — default repository is re-created on boot

Every service boot `ensure_repo`s the configured default repository (`forgevcs`). That is why a
pruned control plane is never fully empty: the next boot recreates it. This is by design and is
documented in `docs/M1-roles.md`; it belongs to the DevOps seat's operational model rather than
the engine's debt.
