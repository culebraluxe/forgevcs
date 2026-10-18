#!/usr/bin/env python3
"""repo_backup.py — zip-archive backup and restore for one ForgeVCS repository.

Backup is full-fidelity: every repository-scoped Postgres row (the Scion metadata tables) is
dumped with `psql COPY`, and every external object byte comes from the DevOps blob-transfer
route. The archive is a plain zip:

    manifest.json          repo, repo_id, refs, row counts, blob count, timestamp
    rows/<table>.copy      psql text COPY output for that table
    blobs.json             {entries: [{key, data(base64)}]} from /blobs/export

Restore recreates the same repository name. ForgeVCS derives repo_id deterministically from the
name (scion-store `compute_repo_id`), so the restored rows and blob keys line up: the script
creates the repository, deletes the bootstrap rows, COPYs the archive back in, and re-uploads the
external blobs. It refuses to overwrite an existing repository unless `--replace` is given.

The metadata COPY needs `psql` on the operator machine; the service needs the owner connection
string it already uses (the shell wrapper exports it as FORGEVCS_BACKUP_DATABASE_URL).
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
import zipfile

# Every table keyed by repo_id (SPEC 5.2). `repos` is handled separately: restore recreates it
# through the service and must not overwrite the repository row.
REPO_TABLES = [
    "landed",
    "ref_log",
    "refs",
    "changeset_children",
    "changesets",
    "object_uploads",
    "object_paths",
    "objects",
    "derived",
    "derive_leases",
    "redactions",
    "counters",
    "audit_log",
]

FORMAT = "forgevcs-repo-backup-v1"


class BackupError(RuntimeError):
    pass


class Vcs:
    def __init__(self, base: str, token: str = "") -> None:
        self.base = base.rstrip("/")
        self.token = token

    def call(self, method: str, path: str, body=None, expect: int = 200):
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.base + path, data=data, method=method)
        if data is not None:
            request.add_header("content-type", "application/json")
        if self.token:
            request.add_header("authorization", "Bearer " + self.token)
        try:
            with urllib.request.urlopen(request, timeout=300) as response:
                return json.loads(response.read() or b"{}")
        except urllib.error.HTTPError as error:
            raise BackupError(
                f"{method} {path}: HTTP {error.code}: {error.read()[:300]!r}"
            ) from error

    def status(self, method: str, path: str):
        try:
            self.call(method, path)
            return 200
        except BackupError as error:
            text = str(error)
            if "HTTP " in text:
                return int(text.split("HTTP ", 1)[1].split(":", 1)[0])
            raise


def mint(base: str, principal: str, secret: str) -> str:
    return Vcs(base).call(
        "POST", "/auth/token", {"service": principal, "secret": secret}
    )["token"]


def psql(url: str, sql: str, stdin: bytes | None = None) -> bytes:
    result = subprocess.run(
        ["psql", url, "-X", "-q", "-v", "ON_ERROR_STOP=1", "-Atc", sql],
        input=stdin,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode != 0:
        raise BackupError(f"psql failed: {result.stderr.decode('utf-8', 'replace')[:400]}")
    return result.stdout


def repo_id(url: str, repo: str) -> int:
    raw = psql(url, f"SELECT repo_id FROM repos WHERE name = '{repo}'").strip()
    if not raw:
        raise BackupError(f"repository {repo!r} is not in the control plane")
    return int(raw)


def backup(base: str, token: str, database_url: str, repo: str, out_path: str) -> dict:
    vcs = Vcs(base, token)
    rid = repo_id(database_url, repo)
    refs = vcs.call("GET", f"/repos/{repo}/refs").get("refs", [])
    blobs = vcs.call("GET", f"/repos/{repo}/blobs/export")
    rows: dict[str, bytes] = {}
    for table in REPO_TABLES:
        rows[table] = psql(
            database_url,
            f"COPY (SELECT * FROM {table} WHERE repo_id = {rid}) TO STDOUT",
        )
    manifest = {
        "format": FORMAT,
        "repo": repo,
        "repo_id": rid,
        "exported_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "refs": refs,
        "row_counts": {
            table: 0 if not data.strip() else data.rstrip(b"\n").count(b"\n") + 1
            for table, data in rows.items()
        },
        "blob_count": blobs.get("count", 0),
    }
    with zipfile.ZipFile(out_path, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("manifest.json", json.dumps(manifest, indent=2))
        archive.writestr("blobs.json", json.dumps(blobs))
        for table, data in rows.items():
            archive.writestr(f"rows/{table}.copy", data)
    return manifest


def read_archive(zip_path: str):
    with zipfile.ZipFile(zip_path) as archive:
        manifest = json.loads(archive.read("manifest.json"))
        blobs = json.loads(archive.read("blobs.json"))
        rows = {table: archive.read(f"rows/{table}.copy") for table in REPO_TABLES}
    return manifest, rows, blobs


def restore(
    base: str,
    token: str,
    database_url: str,
    repo: str,
    zip_path: str,
    replace: bool,
) -> dict:
    manifest, rows, blobs = read_archive(zip_path)
    if manifest.get("format") != FORMAT:
        raise BackupError(f"{zip_path} is not a {FORMAT} archive")
    if manifest["repo"] != repo:
        raise BackupError(
            f"archive is for {manifest['repo']!r}, not {repo!r} (use --repo {manifest['repo']})"
        )
    vcs = Vcs(base, token)
    exists = vcs.status("GET", f"/repos/{repo}/refs") == 200
    if exists and not replace:
        raise BackupError(
            f"repository {repo!r} already exists; pass --replace to delete and restore it"
        )
    if exists:
        vcs.call("DELETE", f"/repos/{repo}", {"confirm": repo})
    vcs.call("POST", f"/repos/{repo}/init", {})
    rid = repo_id(database_url, repo)
    if rid != manifest["repo_id"]:
        raise BackupError(
            f"repo_id mismatch: archive {manifest['repo_id']} vs restored {rid}"
        )
    for table in REPO_TABLES:
        psql(database_url, f"DELETE FROM {table} WHERE repo_id = {rid}")
    for table in REPO_TABLES:
        data = rows[table]
        if data.strip():
            psql(database_url, f"COPY {table} FROM STDIN", stdin=data)
    vcs.call(
        "POST",
        f"/repos/{repo}/blobs/import",
        {"entries": blobs.get("entries", [])},
    )
    return {
        "repo": repo,
        "rows": {table: 0 if not rows[table].strip() else rows[table].rstrip(b"\n").count(b"\n") + 1 for table in REPO_TABLES},
        "blobs": len(blobs.get("entries", [])),
        "manifest": manifest,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("backup", "restore"):
        p = sub.add_parser(name)
        p.add_argument("--base", default="http://127.0.0.1:3994")
        p.add_argument("--devops-secret", required=True)
        p.add_argument("--database-url", default=os.environ.get("FORGEVCS_BACKUP_DATABASE_URL"))
        p.add_argument("--repo", required=True)
        p.add_argument("--zip", required=True)
        if name == "restore":
            p.add_argument("--replace", action="store_true")
    args = parser.parse_args()
    if not args.database_url:
        print("FAIL  --database-url or FORGEVCS_BACKUP_DATABASE_URL is required", file=sys.stderr)
        return 2
    token = mint(args.base, "devops", args.devops_secret)
    try:
        if args.command == "backup":
            manifest = backup(args.base, token, args.database_url, args.repo, args.zip)
            print(
                f"OK  backed up {manifest['repo']} to {args.zip}: "
                f"{sum(manifest['row_counts'].values())} rows, {manifest['blob_count']} blobs"
            )
        else:
            result = restore(
                args.base,
                token,
                args.database_url,
                args.repo,
                args.zip,
                args.replace,
            )
            print(
                f"OK  restored {result['repo']} from {args.zip}: "
                f"{sum(result['rows'].values())} rows, {result['blobs']} blobs"
            )
    except BackupError as error:
        print(f"FAIL  {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
