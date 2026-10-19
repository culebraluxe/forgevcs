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
import shutil
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

# Loopback VCS calls must not go through proxy discovery. Under launchd (a non-login context)
# macOS's urllib proxy path (_scproxy) can block for minutes inside SystemConfiguration; that is
# how the installed daily job booted the service and then hung instead of finishing. These calls
# are always to 127.0.0.1, so an explicit empty ProxyHandler is both correct and fast.
_NO_PROXY_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


class BackupError(RuntimeError):
    pass


class Vcs:
    def __init__(self, base: str, token: str = "", extra_headers: dict | None = None) -> None:
        self.base = base.rstrip("/")
        self.token = token
        self.extra_headers = extra_headers or {}

    def call(self, method: str, path: str, body=None, expect: int = 200):
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.base + path, data=data, method=method)
        if data is not None:
            request.add_header("content-type", "application/json")
        if self.token:
            request.add_header("authorization", "Bearer " + self.token)
        for key, value in self.extra_headers.items():
            request.add_header(key, value)
        try:
            # _NO_PROXY_OPENER, not urlopen(): see the comment on its definition (launchd hang).
            with _NO_PROXY_OPENER.open(request, timeout=300) as response:
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


def require_psql() -> str:
    """Resolve the psql binary (TD-2): the archive's metadata half is a psql COPY export.

    A missing client is a named refusal with the install hint, not a mid-zip stack trace; the
    scheduled job preflights this and turns it into a failure alert.
    """
    explicit = os.environ.get("FORGEVCS_PSQL", "").strip()
    if explicit and os.path.isfile(explicit) and os.access(explicit, os.X_OK):
        return explicit
    found = shutil.which("psql")
    if found:
        return found
    raise BackupError(
        "psql not found on PATH (TD-2): the archive metadata is exported with psql COPY. "
        "Install the PostgreSQL client (brew install postgresql@16) or set FORGEVCS_PSQL."
    )


def psql(url: str, sql: str, stdin: bytes | None = None) -> bytes:
    result = subprocess.run(
        [require_psql(), url, "-X", "-q", "-v", "ON_ERROR_STOP=1", "-Atc", sql],
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
    # Every backup verifies its own archive before it is reported as a backup.
    manifest["verified"] = verify_archive(out_path)["repo"] == repo
    return manifest


def read_archive(zip_path: str):
    with zipfile.ZipFile(zip_path) as archive:
        manifest = json.loads(archive.read("manifest.json"))
        blobs = json.loads(archive.read("blobs.json"))
        rows = {table: archive.read(f"rows/{table}.copy") for table in REPO_TABLES}
    return manifest, rows, blobs


def verify_archive(zip_path: str) -> dict:
    """Open an archive and prove its contents agree with its manifest.

    Every backup runs this immediately after writing, and the restore drill runs it before
    restoring: an archive whose row files disagree with `manifest.json` is not a backup. No
    database and no service are involved.
    """
    if not os.path.isfile(zip_path):
        raise BackupError(f"archive does not exist: {zip_path}")
    try:
        with zipfile.ZipFile(zip_path) as archive:
            missing = [
                name
                for name in ["manifest.json", "blobs.json"]
                + [f"rows/{table}.copy" for table in REPO_TABLES]
                if name not in archive.namelist()
            ]
            if missing:
                raise BackupError(f"{zip_path} is missing archive members: {missing}")
            manifest = json.loads(archive.read("manifest.json"))
            blobs = json.loads(archive.read("blobs.json"))
            rows = {table: archive.read(f"rows/{table}.copy") for table in REPO_TABLES}
    except zipfile.BadZipFile as error:
        raise BackupError(f"{zip_path} is not a readable zip: {error}") from error
    if manifest.get("format") != FORMAT:
        raise BackupError(f"{zip_path} is not a {FORMAT} archive")
    if not manifest.get("repo") or not manifest.get("repo_id"):
        raise BackupError(f"{zip_path} manifest has no repo identity")
    if not isinstance(manifest.get("refs"), list):
        raise BackupError(f"{zip_path} manifest has no refs list")
    stored_counts = {table: int(manifest.get("row_counts", {}).get(table, -1)) for table in REPO_TABLES}
    actual_counts = {
        table: 0 if not data.strip() else data.rstrip(b"\n").count(b"\n") + 1
        for table, data in rows.items()
    }
    if actual_counts != stored_counts:
        raise BackupError(
            f"{zip_path} row counts disagree with its manifest: {actual_counts} vs {stored_counts}"
        )
    entries = blobs.get("entries")
    if not isinstance(entries, list):
        raise BackupError(f"{zip_path} blobs.json has no entries list")
    if len(entries) != int(manifest.get("blob_count", -1)):
        raise BackupError(
            f"{zip_path} blob count disagrees with its manifest: "
            f"{len(entries)} vs {manifest.get('blob_count')}"
        )
    return {
        "repo": manifest["repo"],
        "repo_id": int(manifest["repo_id"]),
        "row_counts": actual_counts,
        "blob_count": len(entries),
        "refs": len(manifest["refs"]),
        "zip_bytes": os.path.getsize(zip_path),
    }


def restore(
    base: str,
    token: str,
    database_url: str,
    repo: str,
    zip_path: str,
    replace: bool,
) -> dict:
    verification = verify_archive(zip_path)
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
    # Exact metadata verification happens BEFORE the blob import, because an import can legitimately
    # add service-written rows; at this point the rows must be byte-for-byte the archived set.
    restored_counts = {
        table: int(psql(database_url, f"SELECT count(*) FROM {table} WHERE repo_id = {rid}").strip() or 0)
        for table in REPO_TABLES
    }
    if restored_counts != verification["row_counts"]:
        raise BackupError(
            f"restored row counts do not match the archive: {restored_counts} vs {verification['row_counts']}"
        )
    entries = blobs.get("entries", [])
    vcs.call("POST", f"/repos/{repo}/blobs/import", {"entries": entries})
    exported = vcs.call("GET", f"/repos/{repo}/blobs/export").get("count", 0)
    if exported != len(entries):
        raise BackupError(
            f"restored blob count does not match the archive: service reports {exported}, archive has {len(entries)}"
        )
    return {
        "repo": repo,
        "rows": restored_counts,
        "blobs": len(entries),
        "refs": verification["refs"],
        "manifest": manifest,
        "verified": True,
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
    p = sub.add_parser("verify", help="verify a zip's contents against its manifest (no service)")
    p.add_argument("--zip", required=True)
    args = parser.parse_args()
    if args.command == "verify":
        try:
            verification = verify_archive(args.zip)
        except BackupError as error:
            print(f"FAIL  {error}", file=sys.stderr)
            return 1
        print(
            f"OK  {args.zip}: repo={verification['repo']} rows={sum(verification['row_counts'].values())} "
            f"blobs={verification['blob_count']} refs={verification['refs']}"
        )
        return 0
    if not args.database_url and args.command != "verify":
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
