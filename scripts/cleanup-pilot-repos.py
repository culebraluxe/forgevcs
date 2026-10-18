#!/usr/bin/env python3
"""cleanup-pilot-repos.py — delete burn-in repositories as the DevOps seat.

Lists the control plane with `GET /repos` and deletes every repository whose name starts with
one of the pilot prefixes. Deletion is the DevOps metadata path: rows in one transaction, shared
content-addressed blobs retained (GC is a separate workstream). Dry run unless `--apply`.

Exit 0 when the plan (dry run) or the deletion completes.
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request

DEFAULT_PREFIXES = ["agent-pilot-", "role-pilot-", "forgevcs-w4-", "forgevcs-w5-"]


class CleanupError(RuntimeError):
    pass


def call(base: str, method: str, path: str, token: str = "", body=None, expect: int = 200):
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(base.rstrip("/") + path, data=data, method=method)
    if data is not None:
        request.add_header("content-type", "application/json")
    if token:
        request.add_header("authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            status, raw = response.status, response.read()
    except urllib.error.HTTPError as error:
        status, raw = error.code, error.read()
    if expect != 0 and status != expect:
        raise CleanupError(f"{method} {path}: expected {expect}, got {status}: {raw[:300]!r}")
    return json.loads(raw) if raw else {}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="http://127.0.0.1:3996")
    parser.add_argument("--devops-secret", required=True)
    parser.add_argument("--prefix", action="append", default=None)
    parser.add_argument("--all", action="store_true", help="delete every repository (legacy prune)")
    parser.add_argument("--apply", action="store_true", help="actually delete (default: dry run)")
    args = parser.parse_args()

    token = call(
        args.base,
        "POST",
        "/auth/token",
        body={"service": "devops", "secret": args.devops_secret},
    )["token"]
    prefixes = args.prefix or DEFAULT_PREFIXES
    repos = call(args.base, "GET", "/repos", token)["repos"]
    if args.all:
        doomed = sorted(repo["name"] for repo in repos)
    else:
        doomed = sorted(
            repo["name"]
            for repo in repos
            if any(repo["name"].startswith(prefix) for prefix in prefixes)
        )
    print(f"INFO  {len(repos)} repositories in the control plane")
    print("INFO  mode      " + ("ALL (legacy prune)" if args.all else ", ".join(prefixes)))
    print(f"INFO  matched {len(doomed)} repositories")
    for name in doomed:
        print(f"  - {name}")
    if not args.apply:
        print("INFO  dry run; pass --apply to delete")
        return 0
    failures = 0
    rows_deleted = 0
    for name in doomed:
        try:
            deleted = call(
                args.base,
                "DELETE",
                f"/repos/{name}",
                token,
                body={"confirm": name},
            )
            rows = sum(
                value for value in deleted["repository"]["rows"].values() if isinstance(value, int)
            )
            rows_deleted += rows
            print(f"DELETED  {name} ({rows} rows)")
        except CleanupError as error:
            failures += 1
            print(f"FAILED   {name}: {error}", file=sys.stderr)
    print(f"OK  deleted {len(doomed) - failures}/{len(doomed)} repositories, {rows_deleted} rows")
    return 1 if failures else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except CleanupError as error:
        print(f"FAIL  {error}", file=sys.stderr)
        raise SystemExit(1)
