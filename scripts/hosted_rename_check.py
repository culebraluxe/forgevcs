#!/usr/bin/env python3
"""hosted_rename_check.py — prove exact-rename landing on the deployed service.

Maestro is the DevOps seat on the hosted deployment. The check creates a repository, lands a
rename (`old.txt -> new.txt`), lands an edit that was staged against the pre-rename base, and
verifies the edit followed the rename. It then deletes the repository.
"""

from __future__ import annotations

import argparse
import base64
import secrets
import sys
import time

import repo_backup

BLOB_A = "b1d3bae0cd810e6f0ca2bdfa4a431f5810e4c1878c0ada9ffcddc706c25020d430"
BLOB_B = "b175ae44830c60dd21712afd5fcf09a4bf86d98d8b3cec75c7b5c0681b8ffb12e9"
CONTENT_A = b"ForgeSC smoke test\n"
CONTENT_B = b"ForgeVCS smoke test\n"


def push_body(parent, path, blob, content, message, delta=None):
    return {
        "objects": [{"id": blob, "data": base64.b64encode(content).decode()}],
        "changeset": {
            "parents": [parent],
            "delta": delta
            or [{"path": path, "op": 1, "mode": 1, "id": blob, "size": len(content)}],
            "meta": {
                "author": "ForgeVCS hosted rename check <rename@example.test>",
                "message": message,
            },
            "change_id": secrets.token_hex(16),
        },
        "ref": "main",
        "mode": "land",
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="https://culebraluxe-forgevcs.vercel.app")
    parser.add_argument("--secret", required=True)
    parser.add_argument("--bypass", required=True)
    args = parser.parse_args()

    vcs = repo_backup.Vcs(args.base, extra_headers={"x-vercel-protection-bypass": args.bypass})
    token = vcs.call(
        "POST", "/auth/token", {"service": "maestro", "secret": args.secret}
    )["token"]
    vcs.token = token
    repo = f"hosted-rename-{time.strftime('%y%m%d%H%M%S')}{secrets.token_hex(2)}"
    checks = 0
    try:
        vcs.call("POST", f"/repos/{repo}/init", {})
        tip = next(
            ref["target"]
            for ref in vcs.call("GET", f"/repos/{repo}/refs")["refs"]
            if ref["name"] == "main"
        )
        base_push = vcs.call(
            "POST", f"/repos/{repo}/push", push_body(tip, "old.txt", BLOB_A, CONTENT_A, "base")
        )
        tip = base_push["ref"]["target"]
        edit = vcs.call(
            "POST",
            f"/repos/{repo}/push",
            {
                **push_body(tip, "old.txt", BLOB_B, CONTENT_B, "edit before rename"),
                "mode": "none",
            },
        )["changeset"]["id"]
        renamed = vcs.call(
            "POST",
            f"/repos/{repo}/push",
            push_body(
                tip,
                "old.txt",
                BLOB_A,
                CONTENT_A,
                "rename",
                delta=[
                    {"path": "new.txt", "op": 1, "mode": 1, "id": BLOB_A, "size": len(CONTENT_A)},
                    {"path": "old.txt", "op": 2},
                ],
            ),
        )
        checks += 1
        followed = vcs.call(
            "POST", f"/repos/{repo}/land", {"ref": "main", "head": edit}
        )
        checks += 1
        head = followed["ref"]["target"]
        new_bytes = base64.b64decode(
            vcs.call("GET", f"/repos/{repo}/file?changeset={head}&path=new.txt")["data"]
        )
        if new_bytes != CONTENT_B:
            raise repo_backup.BackupError(f"new.txt content is {new_bytes!r}, expected B")
        checks += 1
        print(f"PASS  rename landed on hosted main ({renamed['ref']['version']})")
        print("PASS  the staged edit landed after the rename without a conflict")
        print("PASS  the edit follows the rename to new.txt (exact-rename-v1 is enabled)")
    finally:
        try:
            vcs.call("DELETE", f"/repos/{repo}", {"confirm": repo})
            print(f"TEARDOWN  deleted {repo}")
        except repo_backup.BackupError as error:
            print(f"TEARDOWN  could not delete {repo}: {error}", file=sys.stderr)
    print(f"OK  {checks} hosted rename checks passed")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except repo_backup.BackupError as error:
        print(f"FAIL  {error}", file=sys.stderr)
        raise SystemExit(1)
