#!/usr/bin/env python3
"""gc-orphan-blobs.py — sweep object storage for blobs of deleted repositories.

Calls the DevOps-only endpoint `POST /admin/gc/objects`. Dry run by default; `--apply` deletes.
Scion keys are repository-scoped, so only keys whose repo_id is no longer in `repos` are
candidates; malformed keys are reported and never deleted.
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request


class GcError(RuntimeError):
    pass


def call(base: str, method: str, path: str, token: str = "", body=None):
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(base.rstrip("/") + path, data=data, method=method)
    if data is not None:
        request.add_header("content-type", "application/json")
    if token:
        request.add_header("authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(request, timeout=240) as response:
            return json.loads(response.read() or b"{}")
    except urllib.error.HTTPError as error:
        raise GcError(f"{method} {path}: HTTP {error.code}: {error.read()[:300]!r}") from error


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="http://127.0.0.1:3995")
    parser.add_argument("--devops-secret", required=True)
    parser.add_argument("--apply", action="store_true", help="delete (default: dry run)")
    parser.add_argument(
        "--purge-prefix",
        default=None,
        help="delete every key under this explicit legacy prefix instead of the orphan sweep",
    )
    args = parser.parse_args()

    token = call(
        args.base,
        "POST",
        "/auth/token",
        body={"service": "devops", "secret": args.devops_secret},
    )["token"]
    body = {"apply": args.apply}
    if args.purge_prefix:
        body["prefix"] = args.purge_prefix
    report = call(args.base, "POST", "/admin/gc/objects", token, body)["gc"]
    print(f"INFO  keys              {report['keys']}")
    print(f"INFO  live repositories {report['live_repos']}")
    print(f"INFO  orphan keys       {report['orphan_keys']}")
    print(f"INFO  malformed keys    {report['malformed_keys']} (never deleted)")
    print(f"INFO  deleted keys      {report['deleted_keys']}")
    for key in report.get("sample", []):
        print(f"  orphan: {key}")
    for key in report.get("malformed_sample", []):
        print(f"  malformed: {key}")
    if not args.apply:
        print("INFO  dry run; pass --apply to delete the orphan keys")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except GcError as error:
        print(f"FAIL  {error}", file=sys.stderr)
        raise SystemExit(1)
