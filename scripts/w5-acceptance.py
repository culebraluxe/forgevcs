#!/usr/bin/env python3
"""W5 WorkspaceService acceptance client (ForgeVCS V0 work order §9).

Proves the properties the work order names:

- a workspace is created from a ref, written to, diffed, and submitted as a changeset;
- the changeset and its file are visible through the W4 source-control endpoints;
- destroying the workspace does not destroy history: a new workspace at the same ref
  still has the submitted file;
- materialize moves an existing workspace to another changeset and back.

No third-party packages. The same client can run against a local process or the hosted
deployment in W8.
"""

from __future__ import annotations

import argparse
import base64
import json
import secrets
import sys
import urllib.error
import urllib.request

CONTENT = b"workspace history survives disposal\n"


class AcceptanceError(RuntimeError):
    pass


class Client:
    def __init__(self, base: str, token: str, bypass: str = "") -> None:
        self.base = base.rstrip("/")
        self.token = token
        self.bypass = bypass
        self.checks = 0

    def call(self, method: str, path: str, body=None, expect: int = 200):
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.base + path, data=data, method=method)
        if data is not None:
            request.add_header("content-type", "application/json")
        request.add_header("authorization", "Bearer " + self.token)
        if self.bypass:
            request.add_header("x-vercel-protection-bypass", self.bypass)
        try:
            with urllib.request.urlopen(request, timeout=120) as response:
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


def run(base: str, token: str, repo: str, bypass: str = "") -> int:
    client = Client(base, token, bypass)
    prefix = f"/repos/{repo}"

    client.call("POST", prefix + "/init", {})
    refs = client.call("GET", prefix + "/refs")
    base_changeset = next(ref["target"] for ref in refs["refs"] if ref["name"] == "main")
    path = f"workspace-{secrets.token_hex(4)}.txt"

    created = client.call("POST", prefix + "/workspaces", {"ref": "main"})
    workspace = created["workspace_id"]
    client.check("workspace created at main", created["state"]["base"] == base_changeset, str(created))
    client.check("root sparse renders as /", created["state"]["sparse"] == ["/"], str(created["state"]))

    status = client.call("GET", prefix + f"/workspaces/{workspace}")
    client.check("new workspace is clean", status["workspace"]["clean"] is True, str(status))

    write = client.call(
        "PUT",
        prefix + f"/workspaces/{workspace}/file?path={path}",
        {"data": base64.b64encode(CONTENT).decode(), "mode": 1},
    )
    client.check("file written", write["workspace"]["size"] == len(CONTENT), str(write))

    status = client.call("GET", prefix + f"/workspaces/{workspace}")
    client.check("status sees the added file", status["workspace"]["added"] == [path], str(status))

    diff = client.call("GET", prefix + f"/workspaces/{workspace}/diff")
    change = next((c for c in diff["workspace"]["changes"] if c["path"] == path), None)
    client.check("diff sees the added file", change is not None, str(diff))
    lines = [line for hunk in change["hunks"] for line in hunk["lines"]]
    client.check("diff carries the content", any(CONTENT.decode().strip() in line["text"] for line in lines), str(lines))

    read = client.call("GET", prefix + f"/workspaces/{workspace}/file?path={path}")
    client.check("workspace read returns the bytes", base64.b64decode(read["workspace"]["data"]) == CONTENT)

    submitted = client.call(
        "POST",
        prefix + f"/workspaces/{workspace}/submit",
        {"message": "W5 acceptance commit", "ref": "main", "mode": "land"},
    )["workspace"]
    changeset = submitted["changeset"]["changeset"]
    client.check("submit landed", submitted["ref"]["ref"] == "main" and submitted["ref"]["target"] == changeset, str(submitted))

    # The W4 source-control endpoints see the same changeset and file.
    detail = client.call("GET", prefix + f"/changesets/{changeset}")["changeset"]
    client.check("changeset is canonical", detail["id"] == changeset and detail["parents"] == [base_changeset], str(detail))
    client.check("changeset delta has the path", any(entry["path"] == path for entry in detail["delta"]), str(detail))
    file_at = client.call("GET", prefix + f"/file?changeset={changeset}&path={path}")
    client.check("read through W4 returns the bytes", base64.b64decode(file_at["data"]) == CONTENT)

    # Destroy the workspace: the history must remain.
    client.call("DELETE", prefix + f"/workspaces/{workspace}")
    client.call("GET", prefix + f"/workspaces/{workspace}", expect=404)
    client.check("destroyed workspace is 404", True)

    again = client.call("POST", prefix + "/workspaces", {"ref": "main"})["workspace_id"]
    read = client.call("GET", prefix + f"/workspaces/{again}/file?path={path}")
    client.check("history survives workspace disposal", base64.b64decode(read["workspace"]["data"]) == CONTENT)

    # materialize to the old changeset, then back to main.
    moved = client.call(
        "POST",
        prefix + f"/workspaces/{again}/materialize",
        {"changeset": base_changeset, "force": True},
    )
    client.check("materialize moved to the old changeset", moved["workspace"]["state"]["base"] == base_changeset, str(moved))
    client.call("GET", prefix + f"/workspaces/{again}/file?path={path}", expect=404)
    moved = client.call(
        "POST",
        prefix + f"/workspaces/{again}/materialize",
        {"ref": "main", "force": True},
    )
    client.check("materialize moved back to main", moved["workspace"]["state"]["base"] == changeset, str(moved))
    read = client.call("GET", prefix + f"/workspaces/{again}/file?path={path}")
    client.check("file returns after moving back", base64.b64decode(read["workspace"]["data"]) == CONTENT)

    client.call("DELETE", prefix + f"/workspaces/{again}")
    print(f"OK  {client.checks} checks passed for {repo} ({changeset})")
    return client.checks


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="http://127.0.0.1:3000")
    parser.add_argument("--token", required=True)
    parser.add_argument("--repo", default="forgevcs-workspaces")
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
