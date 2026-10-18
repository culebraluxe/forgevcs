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

## TD-2 — Backup operational constraints

**Gap.** A zip archive needs `psql` on the operator machine for the metadata half (the service
only moves object bytes), and the hosted deployment must be recycled after a restore so no
instance keeps a cached engine.

**Why acceptable.** Backups are an operator action, not an in-service API; the drill passes 8/8
(`scripts/backup-acceptance.sh`). The Vercel instances recycle naturally.

**Closing it.** Either a server-side export that reads metadata through the store (no psql) or a
deployment restart hook the restore script can call. Also archive verification (open the zip,
count rows) could run as part of every backup.

## TD-3 — Backup retention and offsite copies

**Gap.** There is a daily schedule now (`scripts/scheduled-backup.sh`, launchd, 02:00, keeps the
last 7 archives per repository) but no offsite copy, no restore spot-check on the schedule, and no
alerting when a backup fails.

**Why acceptable.** The archives cover the full repository state and the restore drill proves they
work; a single-machine archive is a real improvement over nothing and the branch can be re-created
from Neon history if the laptop dies.

**Closing it.** Ship archives to object storage/branch, run the restore drill on a schedule, and
notify on non-zero exit. Retention is one env var (`BACKUP_RETENTION_DAYS`).

## Owned by DevOps, not debt — default repository is re-created on boot

Every service boot `ensure_repo`s the configured default repository (`forgevcs`). That is why a
pruned control plane is never fully empty: the next boot recreates it. This is by design and is
documented in `docs/M1-roles.md`; it belongs to the DevOps seat's operational model rather than
the engine's debt.
