#!/usr/bin/env python3
"""W4 SourceControlService acceptance client (ForgeVCS V0 work order §8).

Drives the REST façade the way a real client would, without the Scion CLI:

    create repo -> add file (A) -> modify file (B) -> read history -> diff A..B
    -> read both file versions -> read the tree -> merge-base -> resolve the ref

It runs against anything that serves the façade: a local process for W4, or the hosted
Vercel deployment for W8. Pass `--base`, `--token` and `--repo`.

Exit 0 only when every assertion passes. No third-party Python packages are used.
"""

from __future__ import annotations

import argparse
import base64
import json
import secrets
import sys
import urllib.error
import urllib.request

# BLAKE3-256 of the two fixed payloads, with Scion's `b1` kind prefix. The service verifies
# every upload against its id, so a mismatch fails the run rather than being papered over.
BLOB_A = "b1d3bae0cd810e6f0ca2bdfa4a431f5810e4c1878c0ada9ffcddc706c25020d430"
BLOB_B = "b175ae44830c60dd21712afd5fcf09a4bf86d98d8b3cec75c7b5c0681b8ffb12e9"
CONTENT_A = b"ForgeSC smoke test\n"
CONTENT_B = b"ForgeVCS smoke test\n"
B64_A = base64.b64encode(CONTENT_A).decode()
B64_B = base64.b64encode(CONTENT_B).decode()


class AcceptanceError(RuntimeError):
    pass


class Client:
    def __init__(self, base: str, token: str, bypass: str = "") -> None:
        self.base = base.rstrip("/")
        self.token = token
        self.bypass = bypass
        self.checks = 0

    def call(self, method: str, path: str, body=None, token: bool = True, expect: int = 200):
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.base + path, data=data, method=method)
        if data is not None:
            request.add_header("content-type", "application/json")
        if token and self.token:
            request.add_header("authorization", "Bearer " + self.token)
        if self.bypass:
            request.add_header("x-vercel-protection-bypass", self.bypass)
        try:
            with urllib.request.urlopen(request, timeout=90) as response:
                status = response.status
                raw = response.read()
        except urllib.error.HTTPError as error:
            status = error.code
            raw = error.read()
        if status != expect:
            raise AcceptanceError(
                f"{method} {path}: expected HTTP {expect}, got {status}: {raw[:500]!r}"
            )
        return json.loads(raw) if raw else {}

    def check(self, name: str, condition: bool, detail: str = "") -> None:
        if not condition:
            raise AcceptanceError(f"{name}: {detail}")
        self.checks += 1
        print(f"PASS  {name}")


def push_body(parent: str, path: str, blob: str, size: int, data: str) -> dict:
    return {
        "objects": [{"id": blob, "data": data}],
        "changeset": {
            "parents": [parent],
            "delta": [{"path": path, "op": 1, "mode": 1, "id": blob, "size": size}],
            "meta": {
                "author": "ForgeVCS acceptance <acceptance@example.test>",
                "message": f"W4 acceptance: {path}",
            },
            "change_id": secrets.token_hex(16),
        },
        "ref": "main",
        "mode": "land",
    }


def run(base: str, token: str, repo: str, bypass: str = "") -> int:
    client = Client(base, token, bypass)
    prefix = f"/repos/{repo}"

    init = client.call("POST", prefix + "/init", {})
    client.check("repository initialized", init.get("status") == "ok", str(init))

    refs = client.call("GET", prefix + "/refs")
    main = next((ref for ref in refs["refs"] if ref["name"] == "main"), None)
    if main is None:
        raise AcceptanceError("no main ref after init")
    parent = main["target"]
    print(f"INFO  main at {parent}")

    # A unique path keeps the client repeatable: A always adds a path, B always modifies it,
    # so a rerun is never an upstream EMPTY_CHANGE.
    path = f"hello-{secrets.token_hex(4)}.txt"
    push_a = client.call(
        "POST",
        prefix + "/push",
        push_body(parent, path, BLOB_A, len(CONTENT_A), B64_A),
    )
    a = push_a["changeset"]["id"]
    client.check("changeset A landed", push_a["ref"]["name"] == "main" and push_a["ref"]["version"] > main["version"])

    push_b = client.call(
        "POST",
        prefix + "/push",
        push_body(a, path, BLOB_B, len(CONTENT_B), B64_B),
    )
    b = push_b["changeset"]["id"]
    client.check("changeset B landed", push_b["ref"]["version"] > push_a["ref"]["version"])
    client.check("B descends from A", push_b["ref"]["target"] == b)

    # Read history through the same endpoint a client uses.
    commits = client.call("GET", prefix + "/commits?ref=main&limit=2")
    tip = commits["commits"][0]
    client.check("history tip is B", tip["id"] == b, str(tip))
    client.check("B.parent == A", a in tip["parents"], str(tip.get("parents")))
    client.check("history includes its parent", commits["commits"][1]["id"] == a)

    # Changeset A's metadata and delta.
    changeset = client.call("GET", prefix + f"/changesets/{a}")["changeset"]
    client.check("changeset A id", changeset["id"] == a)
    client.check("changeset A parent", changeset["parents"] == [parent], str(changeset["parents"]))
    delta = changeset["delta"][0]
    client.check(
        "changeset A delta",
        delta["path"] == path and delta["id"] == BLOB_A and delta["size"] == len(CONTENT_A),
        str(delta),
    )

    # Both file versions, by changeset.
    file_a = client.call("GET", prefix + f"/file?changeset={a}&path={path}")
    file_b = client.call("GET", prefix + f"/file?changeset={b}&path={path}")
    client.check("file at A", base64.b64decode(file_a["data"]) == CONTENT_A, file_a["entry"])
    client.check("file at B", base64.b64decode(file_b["data"]) == CONTENT_B, file_b["entry"])
    client.check("file object ids", file_a["entry"]["id"] == BLOB_A and file_b["entry"]["id"] == BLOB_B)

    # Reconstruct the tree at B.
    tree = client.call("GET", prefix + f"/tree?changeset={b}")
    entry = next((e for e in tree["entries"] if e["name"] == path), None)
    client.check("tree lists the path", entry is not None, str(tree))
    client.check("tree entry is B's blob", entry["id"] == BLOB_B and entry["size"] == len(CONTENT_B))

    # Diff A -> B.
    diff = client.call("GET", prefix + f"/diff?a={a}&b={b}")
    item = next((i for i in diff["items"] if i["path"] == path), None)
    client.check("diff covers the path", item is not None, str(diff))
    lines = [line for hunk in item["hunks"] for line in hunk["lines"]]
    client.check(
        "diff has both revisions",
        any(line.startswith("-ForgeSC smoke test") for line in lines)
        and any(line.startswith("+ForgeVCS smoke test") for line in lines),
        str(lines),
    )

    # Merge base and the ref's final target.
    base = client.call("GET", prefix + f"/merge-base?a={a}&b={b}")
    client.check("merge-base is A", base["bases"] == [a], str(base))
    final_refs = client.call("GET", prefix + "/refs")
    target = next(ref["target"] for ref in final_refs["refs"] if ref["name"] == "main")
    client.check("main points at B", target == b, target)

    # The façade's auth is not optional.
    client.call("POST", prefix + "/push", {"objects": [], "changeset": {}}, token=False, expect=401)
    client.check("push without a token is 401", True)

    print(f"OK  {client.checks} checks passed for {repo} ({a} -> {b})")
    return client.checks


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="http://127.0.0.1:3000")
    parser.add_argument("--token", required=True)
    parser.add_argument("--repo", default="forgevcs-acceptance")
    parser.add_argument("--bypass", default="", help="Vercel automation-bypass header")
    args = parser.parse_args()
    try:
        run(args.base, args.token, args.repo, args.bypass)
    except AcceptanceError as error:
        print(f"FAIL  {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
