#!/usr/bin/env python3
"""backup_all.py — archive every repository and apply the retention window.

For each repository: zip to <dir>/<repo>/<UTC timestamp>.zip (repo_backup.backup), then keep only
the newest BACKUP_RETENTION_DAYS archives for that repository. Runtime target of
scripts/scheduled-backup.sh (launchd, daily 02:00).
"""

from __future__ import annotations

import argparse
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
    args = parser.parse_args()
    if not args.database_url:
        print("FAIL  no database URL", file=sys.stderr)
        return 2

    token = repo_backup.mint(args.base, "devops", args.devops_secret)
    vcs = repo_backup.Vcs(args.base, token)
    repos = [repo["name"] for repo in vcs.call("GET", "/repos").get("repos", [])]
    root = Path(args.dir)
    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    failures = 0
    for repo in repos:
        target_dir = root / repo
        target_dir.mkdir(parents=True, exist_ok=True)
        target = target_dir / f"{stamp}.zip"
        try:
            manifest = repo_backup.backup(args.base, token, args.database_url, repo, str(target))
        except repo_backup.BackupError as error:
            failures += 1
            print(f"FAIL  {repo}: {error}", file=sys.stderr)
            continue
        archives = sorted(target_dir.glob("*.zip"))
        for old in archives[: max(0, len(archives) - args.retention_days)]:
            old.unlink()
        print(
            f"OK    {repo}: {target} "
            f"({sum(manifest['row_counts'].values())} rows, {manifest['blob_count']} blobs, "
            f"kept {min(len(archives), args.retention_days)})"
        )
    print(f"OK    {len(repos) - failures}/{len(repos)} repositories backed up to {root}")
    return 1 if failures else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except repo_backup.BackupError as error:
        print(f"FAIL  {error}", file=sys.stderr)
        raise SystemExit(1)
