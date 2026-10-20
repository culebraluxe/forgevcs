#!/usr/bin/env python3
"""WIRE-1: one-way mirror of a git ref's tree into a ForgeVCS repository.

Reads the git tree with `git ls-tree -r -z` + `git cat-file`, computes the Scion/ForgeVCS blob
ids (BLAKE3 of the raw bytes, Scion text form `b1<64 hex>`), builds a changeset delta against the
mirror's current `main` tree, pushes/lands it through the ForgeVCS REST façade, then pulls the
result back and compares **sha256 per path** with the git bytes. Re-running with an unchanged tree
is a NOOP (the same ref target is left alone).

This is a standalone client: it does not modify `git_publish.rs` and does not make git or
ForgeVCS authoritative over each other. It requires the `blake3` Python package (see
`scripts/wire1-mirror-harness.sh`, which creates a venv).

Exit codes:
  0  mirrored (and verified) or unchanged NOOP (and verified)
  2  usage/configuration error
  3  git read error
  4  ForgeVCS HTTP/API error
  5  verification mismatch (mirror does not byte-match git)
  6  unsupported git tree (submodule/gitlink or non-UTF-8 path)
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import secrets
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from typing import Any

EXIT_OK = 0
EXIT_USAGE = 2
EXIT_GIT = 3
EXIT_HTTP = 4
EXIT_VERIFY = 5
EXIT_UNSUPPORTED = 6

# Scion Mode values (SPEC 4.3.2): 1 file, 2 executable, 3 symlink, 4 tree.
MODE_FILE = 1
MODE_EXECUTABLE = 2
MODE_SYMLINK = 3
MODE_TREE = 4

GIT_MODES = {
    "100644": MODE_FILE,
    "100755": MODE_EXECUTABLE,
    "120000": MODE_SYMLINK,
}
DELTA_PUT = 1
DELTA_DELETE = 2


def uvarint(buf: bytearray, value: int) -> None:
    while True:
        low = value & 0x7F
        value >>= 7
        if value == 0:
            buf.append(low)
            return
        buf.append(low | 0x80)


def canonical_bytes(buf: bytearray, value: bytes) -> None:
    uvarint(buf, len(value))
    buf.extend(value)


def object_id_bytes(text: str) -> bytes:
    """34 raw ObjectId bytes from a Scion text id (`b1<hex>` / `t1<hex>`)."""
    kind = {"b": 1, "t": 2, "c": 3, "m": 4}[text[0]]
    return bytes([1, kind]) + bytes.fromhex(text[2:])


def scion_tree_id(entries: dict[str, "GitEntry"]) -> str:
    """Recompute the Scion Tree id (BLAKE3 of SPEC 4.3.2 canonical encoding) from a git file map.

    This is the tree half of the converter gate: if the computed id equals the server's `tree_id`,
    the git tree and the ForgeVCS tree are the same object, not merely a same-content snapshot.
    """
    root: dict[str, object] = {}
    for path, entry in entries.items():
        node = root
        parts = path.split("/")
        for part in parts[:-1]:
            child = node.get(part)
            if not isinstance(child, dict):
                child = {}
                node[part] = child
            node = child
        node[parts[-1]] = entry

    def encode(node: dict[str, object]) -> str:
        children: list[tuple[str, int, str, int]] = []
        for name in sorted(node, key=lambda value: value.encode("utf-8")):
            value = node[name]
            if isinstance(value, GitEntry):
                children.append((name, value.mode, value.blob_id, value.size))
            else:
                children.append((name, MODE_TREE, encode(value), 0))
        buf = bytearray()
        uvarint(buf, 1)  # format_version
        uvarint(buf, len(children))
        for name, mode, oid, size in children:
            canonical_bytes(buf, name.encode("utf-8"))
            buf.append(mode)
            buf.extend(object_id_bytes(oid))
            uvarint(buf, size)
        uvarint(buf, 0)  # empty extensions
        from blake3 import blake3

        return "t1" + blake3(bytes(buf)).hexdigest()

    return encode(root)


class MirrorError(Exception):
    def __init__(self, message: str, code: int = 1) -> None:
        super().__init__(message)
        self.code = code


@dataclass(frozen=True)
class GitEntry:
    path: str
    mode: int
    blob_id: str
    sha256: str
    size: int
    data: bytes


def git(repo: str, *args: str) -> bytes:
    try:
        return subprocess.run(
            ["git", "-C", repo, *args],
            check=True,
            capture_output=True,
        ).stdout
    except FileNotFoundError as error:
        raise MirrorError(f"git is not available: {error}", EXIT_GIT) from error
    except subprocess.CalledProcessError as error:
        detail = error.stderr.decode("utf-8", "replace").strip()
        raise MirrorError(f"git {' '.join(args)} failed: {detail}", EXIT_GIT) from error


def read_git_tree(repo: str, ref: str) -> tuple[str, dict[str, GitEntry]]:
    commit = git(repo, "rev-parse", "--verify", f"{ref}^{{commit}}").decode().strip()
    raw = git(repo, "ls-tree", "-r", "-z", "--full-tree", commit)
    entries: dict[str, GitEntry] = {}
    for record in raw.split(b"\0"):
        if not record:
            continue
        try:
            meta, raw_path = record.split(b"\t", 1)
            raw_mode, kind, raw_oid = meta.split(b" ")
        except ValueError as error:
            raise MirrorError(f"cannot parse git ls-tree record {record!r}: {error}", EXIT_GIT) from error
        path = raw_path.decode("utf-8")
        if kind != b"blob":
            raise MirrorError(
                f"{path!r} is a git {kind.decode(errors='replace')} (submodule/gitlink); "
                "ForgeVCS has no tree mode for it, so the mirror refuses rather than faking one",
                EXIT_UNSUPPORTED,
            )
        mode_text = raw_mode.decode()
        if mode_text not in GIT_MODES:
            raise MirrorError(
                f"{path!r} has unsupported git mode {mode_text}; supported: 100644/100755/120000",
                EXIT_UNSUPPORTED,
            )
        data = git(repo, "cat-file", "blob", raw_oid.decode())
        # Scion SPEC 4.2: Blob id = b1 + BLAKE3(raw uncompressed bytes).
        from blake3 import blake3  # local import keeps --help working without the dependency

        blob_id = "b1" + blake3(data).hexdigest()
        entries[path] = GitEntry(
            path=path,
            mode=GIT_MODES[mode_text],
            blob_id=blob_id,
            sha256=hashlib.sha256(data).hexdigest(),
            size=len(data),
            data=data,
        )
    return commit, entries


class Client:
    def __init__(self, base: str, token: str, repo: str, bypass: str = "") -> None:
        self.base = base.rstrip("/")
        self.token = token
        self.repo = repo
        self.bypass = bypass
        self.calls = 0

    def call(self, method: str, path: str, body: Any | None = None, expect: tuple[int, ...] = (200,)) -> Any:
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.base + f"/repos/{self.repo}" + path, data=data, method=method)
        if data is not None:
            request.add_header("content-type", "application/json")
        request.add_header("authorization", f"Bearer {self.token}")
        if self.bypass:
            request.add_header("x-vercel-protection-bypass", self.bypass)
        self.calls += 1
        try:
            with urllib.request.urlopen(request, timeout=120) as response:
                status = response.status
                raw = response.read()
        except urllib.error.HTTPError as error:
            status = error.code
            raw = error.read()
        except urllib.error.URLError as error:
            raise MirrorError(f"{method} {path}: cannot reach {self.base}: {error}", EXIT_HTTP) from error
        if status not in expect:
            detail = raw[:800].decode("utf-8", "replace")
            raise MirrorError(f"{method} {path}: expected HTTP {expect}, got {status}: {detail}", EXIT_HTTP)
        return json.loads(raw) if raw else {}

    def init(self) -> None:
        self.call("POST", "/init", {})

    def refs(self) -> dict[str, Any]:
        return self.call("GET", "/refs")

    def tree(self, changeset: str, path: str = "") -> Any:
        suffix = f"&path={urllib.parse.quote(path)}" if path else ""
        return self.call("GET", f"/tree?changeset={urllib.parse.quote(changeset)}{suffix}")

    def file(self, changeset: str, path: str) -> Any:
        return self.call(
            "GET",
            f"/file?changeset={urllib.parse.quote(changeset)}&path={urllib.parse.quote(path)}",
        )

    def push(self, body: dict[str, Any]) -> Any:
        return self.call("POST", "/push", body)


def remote_tree(client: Client, changeset: str) -> dict[str, dict[str, Any]]:
    """Recursively enumerate the flat file map of a changeset's tree."""
    entries: dict[str, dict[str, Any]] = {}

    def walk(prefix: str) -> None:
        for entry in client.tree(changeset, prefix)["entries"]:
            name = entry["name"]
            path = f"{prefix}/{name}" if prefix else name
            if entry["mode"] == MODE_TREE:
                walk(path)
            else:
                entries[path] = {"mode": entry["mode"], "id": entry["id"], "size": entry["size"]}

    walk("")
    return entries


def build_delta(
    remote: dict[str, dict[str, Any]], git_entries: dict[str, GitEntry]
) -> list[dict[str, Any]]:
    """Delta from the mirror's current tree to the git tree, sorted bytewise by path."""
    changes: list[dict[str, Any]] = []
    for path, entry in git_entries.items():
        previous = remote.get(path)
        if previous and previous["id"] == entry.blob_id and previous["mode"] == entry.mode:
            continue
        changes.append(
            {
                "path": path,
                "op": DELTA_PUT,
                "mode": entry.mode,
                "id": entry.blob_id,
                "size": entry.size,
            }
        )
    for path in remote:
        if path not in git_entries:
            changes.append({"path": path, "op": DELTA_DELETE, "mode": MODE_FILE, "id": "", "size": 0})
    # SPEC 4.3.4: delta entries are strictly ascending by path bytes, puts and deletes interleaved.
    changes.sort(key=lambda item: item["path"].encode("utf-8"))
    return changes


def verify(client: Client, target: str, git_entries: dict[str, GitEntry]) -> dict[str, Any]:
    remote = remote_tree(client, target)
    mismatches: list[str] = []
    missing = sorted(set(git_entries) - set(remote))
    extra = sorted(set(remote) - set(git_entries))
    for path in missing:
        mismatches.append(f"{path}: not present in the mirror")
    for path in extra:
        mismatches.append(f"{path}: present in the mirror but not in git")
    checked = 0
    for path, entry in git_entries.items():
        pulled = remote.get(path)
        if pulled is None:
            continue
        if pulled["mode"] != entry.mode:
            mismatches.append(f"{path}: mode mirror={pulled['mode']} git={entry.mode}")
        payload = client.file(target, path)
        data = base64.b64decode(payload["data"])
        pulled_sha = hashlib.sha256(data).hexdigest()
        if pulled_sha != entry.sha256:
            mismatches.append(f"{path}: sha256 mirror={pulled_sha} git={entry.sha256}")
        if payload["entry"] is not None and payload["entry"].get("id") != entry.blob_id:
            mismatches.append(
                f"{path}: blob id mirror={payload['entry'].get('id')} git={entry.blob_id}"
            )
        checked += 1
    return {
        "verified": not mismatches,
        "checked_paths": checked,
        "missing": missing,
        "extra": extra,
        "mismatches": mismatches,
    }


def mirror(args: argparse.Namespace) -> dict[str, Any]:
    commit, git_entries = read_git_tree(args.git_repo, args.git_ref)
    client = Client(args.base, args.token, args.repo, args.bypass)
    client.init()
    refs = client.refs()["refs"]
    main = next((ref for ref in refs if ref["name"] == "main"), None)
    if main is None:
        raise MirrorError("repository has no `main` ref after init", EXIT_HTTP)
    current_target = main["target"]
    root_tree_id = scion_tree_id(git_entries)
    remote = remote_tree(client, current_target)
    delta = build_delta(remote, git_entries)
    unchanged = not delta
    counts = {"added": 0, "modified": 0, "deleted": 0, "unchanged": 0}
    for item in delta:
        if item["op"] == DELTA_DELETE:
            counts["deleted"] += 1
        elif item["path"] in remote:
            counts["modified"] += 1
        else:
            counts["added"] += 1
    counts["unchanged"] = len(git_entries) - counts["added"] - counts["modified"]

    report: dict[str, Any] = {
        "repo": args.repo,
        "git_ref": args.git_ref,
        "git_commit": commit,
        "base_target": current_target,
        "mode": "noop" if unchanged else "mirrored",
        "counts": counts,
        "git_paths": len(git_entries),
        "root_tree_id": root_tree_id,
        "forgevcs_calls": 0,
    }
    if args.dry_run:
        report["dry_run"] = True
        report["planned_delta"] = delta
        report["forgevcs_calls"] = client.calls
        return report
    changesets: list[str] = []
    # Vercel caps a request body (~4.5 MB), so a large tree cannot travel in one push. Split the delta
    # into batches whose encoded object bytes stay under --max-push-bytes; each batch lands on main and
    # becomes the parent of the next. Deletes travel with their own batch and carry no objects.
    if not unchanged:
        by_path = {path: entry for path, entry in git_entries.items()}
        max_bytes = max(1, args.max_push_bytes)
        pending = list(delta)
        while pending:
            batch: list[dict[str, Any]] = []
            batch_bytes = 0
            rest: list[dict[str, Any]] = []
            seen_blobs: set[str] = set()
            for item in pending:
                entry = by_path.get(item["path"])
                size = 0
                if item["op"] == DELTA_PUT and entry is not None and entry.blob_id not in seen_blobs:
                    size = (len(entry.data) + 2) // 3 * 4 + len(entry.blob_id) + 64
                if batch and batch_bytes + size > max_bytes:
                    rest.append(item)
                    continue
                batch.append(item)
                batch_bytes += size
                if item["op"] == DELTA_PUT and entry is not None:
                    seen_blobs.add(entry.blob_id)
            objects = []
            for item in batch:
                entry = by_path.get(item["path"])
                if item["op"] == DELTA_PUT and entry is not None and entry.blob_id in seen_blobs:
                    objects.append({"id": entry.blob_id, "data": base64.b64encode(entry.data).decode()})
                    seen_blobs.discard(entry.blob_id)
            push = client.push(
                {
                    "objects": objects,
                    "changeset": {
                        "parents": [current_target],
                        "delta": batch,
                        "meta": {
                            "author": "Forge mirror <mirror@forgevcs.local>",
                            "message": f"WIRE-1 mirror {args.git_ref}@{commit[:12]}"
                                       + (f" batch {len(changesets) + 1}" if rest else ""),
                        },
                        "change_id": secrets.token_hex(16),
                    },
                    "ref": "main",
                    "mode": "land",
                }
            )
            changesets.append(push["changeset"]["id"])
            current_target = push["ref"]["target"]
            report["ref_version"] = push["ref"]["version"]
            pending = rest
        report["changesets"] = changesets
        report["changeset"] = changesets[-1] if changesets else None
        report["target"] = current_target
    else:
        report["target"] = current_target
    verification = verify(client, report["target"], git_entries)
    server_tree_id = client.tree(report["target"], "")["tree_id"]
    verification["root_tree_id"] = root_tree_id
    verification["server_root_tree_id"] = server_tree_id
    verification["tree_id_reproduced"] = server_tree_id == root_tree_id
    if not verification["tree_id_reproduced"]:
        verification["verified"] = False
        verification["mismatches"].append(
            f"root tree id: computed={root_tree_id} server={server_tree_id}"
        )
    report.update(verification)
    report["forgevcs_calls"] = client.calls
    if not verification["verified"]:
        raise MirrorError(
            "mirror verification failed:\n  " + "\n  ".join(verification["mismatches"]),
            EXIT_VERIFY,
        )
    return report


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--git-repo", required=True, help="path to the git worktree/bare repo")
    parser.add_argument("--git-ref", default="main", help="git ref to mirror (default: main)")
    parser.add_argument("--base", default=os.environ.get("FORGEVCS_BASE_URL", "http://127.0.0.1:3000"))
    parser.add_argument("--repo", default=os.environ.get("FORGEVCS_REPO", "forgevcs"))
    parser.add_argument("--token", default=os.environ.get("FORGEVCS_TOKEN", ""))
    parser.add_argument("--bypass", default=os.environ.get("FORGEVCS_BYPASS", ""),
                        help="Vercel protection-bypass header for hosted deployments")
    parser.add_argument("--max-push-bytes", type=int, default=3_000_000,
                        help="max encoded object bytes per changeset push (Vercel body limit)")
    parser.add_argument("--dry-run", action="store_true", help="print the delta/would-do, no write")
    parser.add_argument("--json", action="store_true", help="print the report JSON")
    parser.add_argument("--json-report", help="write the report JSON to this file")
    args = parser.parse_args(argv)
    if not args.token:
        print("error: no ForgeVCS token (pass --token or FORGEVCS_TOKEN)", file=sys.stderr)
        return EXIT_USAGE
    try:
        report = mirror(args)
    except MirrorError as error:
        print(f"FAIL  {error}", file=sys.stderr)
        return error.code
    except Exception as error:  # noqa: BLE001 - the CLI boundary turns anything else into a clear failure
        print(f"FAIL  unexpected mirror error: {error}", file=sys.stderr)
        return 1
    if args.json_report:
        with open(args.json_report, "w", encoding="utf-8") as handle:
            json.dump(report, handle, indent=2, sort_keys=True)
            handle.write("\n")
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(
            f"{report['mode'].upper()}  {report['repo']} {report['git_ref']}@{report['git_commit'][:12]}"
            f"  target={report['target']}  paths={report['git_paths']}"
            f"  added={report['counts']['added']} modified={report['counts']['modified']}"
            f" deleted={report['counts']['deleted']} unchanged={report['counts']['unchanged']}"
            f"  verified={report['verified']} sha256_paths={report['checked_paths']}"
        )
        if report.get("changeset"):
            print(f"       changeset={report['changeset']}")
    return EXIT_OK


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
