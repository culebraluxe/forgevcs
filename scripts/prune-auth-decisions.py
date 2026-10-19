#!/usr/bin/env python3
"""prune-auth-decisions.py — retention for `forgevcs_auth_decisions` (R6).

The W6 A/B harness writes one row per evaluated authorization decision. They are audit evidence,
not the source of truth, and the table is indexed on `at`; retention is a single delete of rows
older than the window. Defaults to DRY RUN: it prints counts and deletes nothing.

    scripts/prune-auth-decisions.sh                      # dry run, 90 days
    scripts/prune-auth-decisions.sh --days 30 --json     # dry run, machine readable
    scripts/prune-auth-decisions.sh --apply --confirm    # actually prune (explicit)

Safety/window knobs:
  * `--days N` (or FORGEVCS_AUTH_DECISIONS_RETENTION_DAYS), default 90, minimum 1;
  * `--apply` deletes in batches of `--batch-size` only with `--confirm`;
  * no migration runner and no service restart: the table is created at boot and the DELETE is
    one idempotent statement batch, safe to run while a single instance is serving;
  * this is an operator tool — run the dry run first and check the counts.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import subprocess
import sys

TABLE = "forgevcs_auth_decisions"
DEFAULT_RETENTION_DAYS = 90
DEFAULT_BATCH_SIZE = 5000


class PruneError(RuntimeError):
    """The prune could not run (connection/table/query failure)."""


def database_url(explicit: str | None) -> str:
    url = explicit or os.environ.get("FORGEVCS_DATABASE_URL") or os.environ.get("DATABASE_URL")
    if not url:
        raise PruneError(
            "no database URL: pass --database-url or set FORGEVCS_DATABASE_URL/DATABASE_URL"
        )
    return url


def psql(url: str, sql: str) -> str:
    completed = subprocess.run(
        ["psql", url, "-v", "ON_ERROR_STOP=1", "-tAc", sql],
        capture_output=True,
        text=True,
        timeout=180,
    )
    if completed.returncode != 0:
        raise PruneError(completed.stderr.strip()[:600] or "psql failed")
    return completed.stdout.strip()


def scalar(url: str, sql: str) -> int:
    raw = psql(url, sql)
    try:
        return int(raw.splitlines()[0] if raw else 0)
    except ValueError as error:
        raise PruneError(f"expected a number from psql, got {raw!r}") from error


def timestamp(url: str, sql: str) -> str | None:
    raw = psql(url, sql)
    return raw or None


def counts(url: str, days: int) -> dict:
    total = scalar(url, f"SELECT count(*) FROM {TABLE}")
    older = scalar(
        url,
        f"SELECT count(*) FROM {TABLE} WHERE at < now() - interval '{days} days'",
    )
    oldest = timestamp(url, f"SELECT min(at)::text FROM {TABLE}")
    newest = timestamp(url, f"SELECT max(at)::text FROM {TABLE}")
    cutoff = timestamp(url, f"SELECT (now() - interval '{days} days')::text")
    return {
        "table": TABLE,
        "retention_days": days,
        "cutoff": cutoff,
        "rows_total": total,
        "rows_older_than_window": older,
        "rows_kept": total - older,
        "oldest_row": oldest,
        "newest_row": newest,
    }


def apply(url: str, days: int, batch_size: int) -> dict:
    deleted = 0
    batches = 0
    while True:
        statement = (
            f"WITH doomed AS ("
            f"SELECT id FROM {TABLE} WHERE at < now() - interval '{days} days' "
            f"ORDER BY at LIMIT {batch_size}), "
            f"deleted AS (DELETE FROM {TABLE} d USING doomed WHERE d.id = doomed.id RETURNING 1) "
            f"SELECT count(*) FROM deleted"
        )
        removed = scalar(url, statement)
        batches += 1
        deleted += removed
        if removed < batch_size:
            break
    return {"batches": batches, "rows_deleted": deleted, **counts(url, days)}


def report(result: dict, *, json_output: bool) -> None:
    if json_output:
        print(json.dumps(result, indent=2, sort_keys=True))
        return
    print("auth-decision retention")
    print(f"  table:                 {result['table']}")
    print(f"  window:                {result['retention_days']} days (cutoff {result['cutoff']})")
    print(f"  rows total:            {result['rows_total']}")
    print(f"  older than window:     {result['rows_older_than_window']}")
    print(f"  oldest row:            {result['oldest_row']}")
    print(f"  newest row:            {result['newest_row']}")
    if "rows_deleted" in result:
        print(f"  deleted:               {result['rows_deleted']} in {result['batches']} batch(es)")
        print(f"  rows kept:             {result['rows_kept']}")
    else:
        print("  mode:                  DRY RUN — nothing deleted (--apply --confirm to prune)")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--days",
        type=int,
        default=int(
            os.environ.get("FORGEVCS_AUTH_DECISIONS_RETENTION_DAYS", DEFAULT_RETENTION_DAYS)
        ),
        help="retention window in days (default 90, min 1)",
    )
    parser.add_argument("--database-url", default=None, help="override the database URL")
    parser.add_argument("--apply", action="store_true", help="delete; requires --confirm")
    parser.add_argument(
        "--confirm",
        action="store_true",
        help="confirm the delete (double opt-in for --apply)",
    )
    parser.add_argument(
        "--batch-size", type=int, default=DEFAULT_BATCH_SIZE, help="delete batch size (default 5000)"
    )
    parser.add_argument("--json", action="store_true", help="print machine-readable JSON")
    args = parser.parse_args()

    if args.days < 1:
        print("--days must be >= 1", file=sys.stderr)
        return 2
    if args.batch_size < 1:
        print("--batch-size must be >= 1", file=sys.stderr)
        return 2
    if args.apply and not args.confirm:
        print(
            "refusing to delete without --confirm (run the dry run first, then "
            "--apply --confirm)",
            file=sys.stderr,
        )
        return 2

    try:
        url = database_url(args.database_url)
        result = apply(url, args.days, args.batch_size) if args.apply else counts(url, args.days)
    except PruneError as error:
        print(f"PRUNE=FAIL {error}", file=sys.stderr)
        return 1
    except subprocess.TimeoutExpired:
        print("PRUNE=FAIL psql timed out", file=sys.stderr)
        return 1

    result["mode"] = "apply" if args.apply else "dry-run"
    result["generated_at"] = dt.datetime.now(dt.UTC).isoformat(timespec="seconds")
    report(result, json_output=args.json)
    return 0


if __name__ == "__main__":
    sys.exit(main())
