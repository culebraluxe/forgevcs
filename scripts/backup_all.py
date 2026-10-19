#!/usr/bin/env python3
"""backup_all.py — archive every repository and apply the retention window.

For each repository: zip to <dir>/<repo>/<UTC timestamp>.zip (repo_backup.backup), VERIFY the
archive's contents against its manifest, then keep only the newest BACKUP_RETENTION_DAYS daily
archives for that repository. Runtime target of scripts/scheduled-backup.sh (launchd, daily 02:00).

Mac-local zip is the accepted offsite copy (owner decision, 2026-10-18); Neon has its own backups.
Writes a machine-readable summary (--summary) for the scheduled job's status/evidence.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from pathlib import Path

import repo_backup


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="http://127.0.0.1:3993")
    parser.add_argument("--devops-secret", required=True)
    parser.add_argument("--dir", default=os.environ.get("FORGEVCS_BACKUP_DIR", str(Path.home() / "Documents/forgevcs-backups")))
    parser.add_argument("--database-url", default=os.environ.get("FORGEVCS_BACKUP_DATABASE_URL"))
    parser.add_argument(
        "--retention-days",
        type=int,
        default=int(os.environ.get("BACKUP_RETENTION_DAYS", "7")),
    )
    parser.add_argument("--summary", default=None, help="write a JSON run summary here")
    args = parser.parse_args()
    if not args.database_url:
        print("FAIL  no database URL", file=sys.stderr)
        return 2
    if args.retention_days < 1:
        print("FAIL  --retention-days must be >= 1", file=sys.stderr)
        return 2

    token = repo_backup.mint(args.base, "devops", args.devops_secret)
    vcs = repo_backup.Vcs(args.base, token)
    repos = [repo["name"] for repo in vcs.call("GET", "/repos").get("repos", [])]
    root = Path(args.dir)
    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    failures = 0
    entries: list[dict] = []
    total_rows = 0
    total_blobs = 0
    verified = 0
    pruned = 0
    for repo in repos:
        target_dir = root / repo
        target_dir.mkdir(parents=True, exist_ok=True)
        target = target_dir / f"{stamp}.zip"
        try:
            manifest = repo_backup.backup(args.base, token, args.database_url, repo, str(target))
            # Every archive is verified against its own manifest before it counts as a backup.
            verification = repo_backup.verify_archive(str(target))
        except repo_backup.BackupError as error:
            failures += 1
            entries.append({"repo": repo, "ok": False, "error": str(error)})
            print(f"FAIL  {repo}: {error}", file=sys.stderr)
            continue
        verified += 1
        total_rows += sum(manifest["row_counts"].values())
        total_blobs += manifest["blob_count"]
        archives = sorted(target_dir.glob("*.zip"))
        pruned_here = 0
        for old in archives[: max(0, len(archives) - args.retention_days)]:
            if old == target:
                continue
            old.unlink()
            pruned_here += 1
        pruned += pruned_here
        entries.append(
            {
                "repo": repo,
                "ok": True,
                "archive": str(target),
                "zip_bytes": verification["zip_bytes"],
                "rows": sum(verification["row_counts"].values()),
                "row_counts": verification["row_counts"],
                "blobs": verification["blob_count"],
                "refs": verification["refs"],
                "verified": True,
                "kept": min(len(archives), args.retention_days),
                "pruned": pruned_here,
            }
        )
        print(
            f"OK    {repo}: {target} "
            f"({sum(manifest['row_counts'].values())} rows, {manifest['blob_count']} blobs, "
            f"verified, kept {min(len(archives), args.retention_days)})"
        )
    ok_count = len(repos) - failures
    print(
        f"OK    {ok_count}/{len(repos)} repositories backed up and verified to {root} "
        f"(retention {args.retention_days}d, pruned {pruned})"
    )
    summary = {
        "format": "forgevcs-backup-summary-v1",
        "finished_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "backup_dir": str(root),
        "retention_days": args.retention_days,
        "repositories": len(repos),
        "ok": ok_count,
        "failed": failures,
        "verified": verified,
        "total_rows": total_rows,
        "total_blobs": total_blobs,
        "pruned": pruned,
        "entries": entries,
    }
    if args.summary:
        Path(args.summary).parent.mkdir(parents=True, exist_ok=True)
        Path(args.summary).write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(f"SUMMARY {args.summary}")
    return 1 if failures else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except repo_backup.BackupError as error:
        print(f"FAIL  {error}", file=sys.stderr)
        raise SystemExit(1)
