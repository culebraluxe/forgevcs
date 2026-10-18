#!/usr/bin/env python3
"""backup_acceptance.py — the backup/restore drill for one repository.

Creates a repository with history, zips it, deletes it, restores it, and proves refs and file
bytes survived. Then it deletes the repository again and runs the orphan GC so the drill leaves
no rows and no blobs behind. Run through scripts/backup-acceptance.sh (it boots the service).
"""

from __future__ import annotations

import argparse
import base64
import json
import secrets
import sys
import time

import repo_backup

BLOB_A = "b1d3bae0cd810e6f0ca2bdfa4a431f5810e4c1878c0ada9ffcddc706c25020d430"
BLOB_B = "b175ae44830c60dd21712afd5fcf09a4bf86d98d8b3cec75c7b5c0681b8ffb12e9"
CONTENT_A = b"ForgeSC smoke test\n"
CONTENT_B = b"ForgeVCS smoke test\n"


class Check:
    def __init__(self) -> None:
        self.count = 0

    def ok(self, name: str, condition: bool, detail: str = "") -> None:
        if not condition:
            raise repo_backup.BackupError(f"{name}: {detail}")
        self.count += 1
        print(f"PASS  {name}")


def push_body(parent: str, path: str, blob: str, content: bytes, message: str) -> dict:
    return {
        "objects": [{"id": blob, "data": base64.b64encode(content).decode()}],
        "changeset": {
            "parents": [parent],
            "delta": [{"path": path, "op": 1, "mode": 1, "id": blob, "size": len(content)}],
            "meta": {
                "author": "ForgeVCS backup drill <backup@example.test>",
                "message": message,
            },
            "change_id": secrets.token_hex(16),
        },
        "ref": "main",
        "mode": "land",
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="http://127.0.0.1:3994")
    parser.add_argument("--devops-secret", required=True)
    parser.add_argument("--database-url", default=None)
    parser.add_argument("--zip", default="/tmp/forgevcs-backup-acceptance.zip")
    args = parser.parse_args()
    database_url = args.database_url or __import__("os").environ["FORGEVCS_BACKUP_DATABASE_URL"]

    token = repo_backup.mint(args.base, "devops", args.devops_secret)
    vcs = repo_backup.Vcs(args.base, token)
    repo = f"accept-backup-{time.strftime('%y%m%d%H%M%S')}{secrets.token_hex(2)}"
    check = Check()

    try:
        vcs.call("POST", f"/repos/{repo}/init", {})
        check.ok("setup: repository created", True)
        tip, _ = next(
            (ref["target"], ref["version"])
            for ref in vcs.call("GET", f"/repos/{repo}/refs")["refs"]
            if ref["name"] == "main"
        )
        first = vcs.call(
            "POST", f"/repos/{repo}/push", push_body(tip, "notes/a.txt", BLOB_A, CONTENT_A, "A")
        )
        second = vcs.call(
            "POST",
            f"/repos/{repo}/push",
            push_body(
                first["ref"]["target"], "notes/b.txt", BLOB_B, CONTENT_B, "B"
            ),
        )
        head = second["ref"]["target"]
        check.ok("setup: two changesets landed", bool(head))
        before_refs = vcs.call("GET", f"/repos/{repo}/refs")["refs"]
        before_a = base64.b64decode(
            vcs.call("GET", f"/repos/{repo}/file?changeset={head}&path=notes/a.txt")["data"]
        )
        check.ok("setup: file content readable", before_a == CONTENT_A)

        manifest = repo_backup.backup(args.base, token, database_url, repo, args.zip)
        check.ok(
            "backup: zip written with rows and blobs",
            manifest["row_counts"]["changesets"] >= 3 and manifest["blob_count"] >= 1,
            str(manifest["row_counts"]),
        )

        vcs.call("DELETE", f"/repos/{repo}", {"confirm": repo})
        check.ok(
            "drill: repository deleted before restore",
            vcs.status("GET", f"/repos/{repo}/refs") == 404,
        )

        result = repo_backup.restore(args.base, token, database_url, repo, args.zip, replace=False)
        check.ok(
            "restore: rows and blobs reimported",
            result["rows"]["changesets"] >= 3 and result["blobs"] >= 1,
            str(result["rows"]),
        )
        after_refs = vcs.call("GET", f"/repos/{repo}/refs")["refs"]
        before = {ref["name"]: (ref["target"], ref["version"]) for ref in before_refs}
        after = {ref["name"]: (ref["target"], ref["version"]) for ref in after_refs}
        check.ok("restore: refs and versions match the archive", before == after, f"{before} vs {after}")
        after_a = base64.b64decode(
            vcs.call("GET", f"/repos/{repo}/file?changeset={head}&path=notes/a.txt")["data"]
        )
        after_b = base64.b64decode(
            vcs.call("GET", f"/repos/{repo}/file?changeset={head}&path=notes/b.txt")["data"]
        )
        check.ok(
            "restore: both file versions survived",
            after_a == CONTENT_A and after_b == CONTENT_B,
        )
    finally:
        # Teardown: delete the repository, then collect its orphan blobs.
        if vcs.status("GET", f"/repos/{repo}/refs") == 200:
            vcs.call("DELETE", f"/repos/{repo}", {"confirm": repo})
        gc = vcs.call("POST", "/admin/gc/objects", {"apply": True})["gc"]
        print(f"TEARDOWN  deleted {repo}; GC removed {gc['deleted_keys']} orphan keys")

    print(f"OK  {check.count} backup/restore checks passed ({args.zip})")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except repo_backup.BackupError as error:
        print(f"FAIL  {error}", file=sys.stderr)
        raise SystemExit(1)
