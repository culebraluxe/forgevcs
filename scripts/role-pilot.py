#!/usr/bin/env python3
"""role-pilot.py — prove the DevOps/worker authority split.

Two principals against one ForgeVCS engine and one strict policy
(`scripts/pilot/acl-roles.toml`):

  worker (agent-a)  checkout/read/diff/merge-base, push to its own lane ref
  devops            create repository, provision refs, advance `main`

Checks, in order:

  identity      both principals mint a token and /auth/whoami names them
  lifecycle     the DevOps seat creates a repository; the worker's attempt is 403
  refs          the DevOps seat creates `lane/a`; the worker cannot create a ref (403)
  check-in      the worker lands a changeset on its own lane ref
  merge advance the worker's push to `main` is 403; the DevOps seat lands the
                worker's head on `main`, and the worker's file is there
  queue         a conflicting merge is escalated by the worker; a worker cannot
                list or claim it; DevOps sees, claims and resolves it, and the
                resolved item leaves the open list
  basic level   the worker can read refs, compute a merge base, and create a workspace
  deletion      there is no repository deletion endpoint yet (recorded, not graded) —
                when one is built it must take the same admin seat

Exit 0 only when every graded check passes. The run creates two repositories (one denied
attempt creates none) and never deletes them; see docs/M1-roles.md.
"""

from __future__ import annotations

import argparse
import base64
import json
import secrets
import sys
import time
import urllib.error
import urllib.request

BLOB_A = "b1d3bae0cd810e6f0ca2bdfa4a431f5810e4c1878c0ada9ffcddc706c25020d430"
BLOB_B = "b175ae44830c60dd21712afd5fcf09a4bf86d98d8b3cec75c7b5c0681b8ffb12e9"
CONTENT_A = b"ForgeSC smoke test\n"
CONTENT_B = b"ForgeVCS smoke test\n"
B64_A = base64.b64encode(CONTENT_A).decode()
B64_B = base64.b64encode(CONTENT_B).decode()
DEFAULT_REF = "main"


class RoleError(RuntimeError):
    pass


class Client:
    def __init__(self, base: str, token: str = "") -> None:
        self.base = base.rstrip("/")
        self.token = token
        self.checks = 0

    def _request(self, method: str, path: str, body=None, token: bool = True):
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.base + path, data=data, method=method)
        if data is not None:
            request.add_header("content-type", "application/json")
        if token and self.token:
            request.add_header("authorization", "Bearer " + self.token)
        try:
            with urllib.request.urlopen(request, timeout=120) as response:
                return response.status, response.read()
        except urllib.error.HTTPError as error:
            return error.code, error.read()

    def call(self, method: str, path: str, body=None, expect: int = 200):
        status, raw = self._request(method, path, body)
        if status != expect:
            raise RoleError(f"{method} {path}: expected {expect}, got {status}: {raw[:500]!r}")
        return json.loads(raw) if raw else {}

    def status(self, method: str, path: str, body=None):
        status, raw = self._request(method, path, body)
        try:
            parsed = json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            parsed = {"raw": raw.decode("utf-8", "replace")[:300]}
        return status, parsed

    def check(self, name: str, condition: bool, detail: str = "") -> None:
        if not condition:
            raise RoleError(f"{name}: {detail}")
        self.checks += 1
        print(f"PASS  {name}")


def mint(base: str, principal: str, secret: str) -> str:
    token = Client(base).call(
        "POST", "/auth/token", {"service": principal, "secret": secret}
    )["token"]
    if not token:
        raise RoleError(f"{principal}: no token minted")
    return token


def push_body(
    parent: str,
    path: str,
    message: str,
    ref: str,
    *,
    blob: str = BLOB_A,
    content: bytes = CONTENT_A,
    mode: str = "land",
) -> dict:
    return {
        "objects": [{"id": blob, "data": base64.b64encode(content).decode()}],
        "changeset": {
            "parents": [parent],
            "delta": [{"path": path, "op": 1, "mode": 1, "id": blob, "size": len(content)}],
            "meta": {
                "author": "ForgeVCS role pilot <role@example.test>",
                "message": message,
            },
            "change_id": secrets.token_hex(16),
        },
        "ref": ref,
        "mode": mode,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="http://127.0.0.1:3997")
    parser.add_argument("--agents", required=True, help="principal=secret,principal=secret")
    args = parser.parse_args()

    secrets_by_name: dict[str, str] = {}
    for item in args.agents.split(","):
        principal, _, secret = item.partition("=")
        if not principal or not secret:
            print(f"FAIL  malformed --agents entry: {item}", file=sys.stderr)
            return 2
        secrets_by_name[principal.strip()] = secret.strip()
    if set(secrets_by_name) != {"agent-a", "devops"}:
        print("FAIL  --agents must name agent-a and devops", file=sys.stderr)
        return 2

    base = args.base
    worker = Client(base, mint(base, "agent-a", secrets_by_name["agent-a"]))
    devops = Client(base, mint(base, "devops", secrets_by_name["devops"]))
    stamp = time.strftime("%y%m%d%H%M%S") + secrets.token_hex(2)
    repo = f"role-pilot-{stamp}"

    print(f"INFO  engine {base}")
    print(f"INFO  repo   {repo}")

    # --- identity -------------------------------------------------------------------------
    for name, client in (("agent-a", worker), ("devops", devops)):
        who = client.call("GET", "/auth/whoami")
        client.check(f"identity: /auth/whoami names {name}", name in who.get("principal", ""), str(who))

    # --- repository lifecycle is the DevOps seat ------------------------------------------
    created = devops.call("POST", f"/repos/{repo}/init", {})
    devops.check(
        "lifecycle: the DevOps seat created the repository",
        created.get("created") is True,
        str(created),
    )
    denied_status, denied = worker.status("POST", f"/repos/{repo}-worker/init", {})
    worker.check(
        "lifecycle: a worker cannot create a repository (403)",
        denied_status == 403,
        f"HTTP {denied_status} {str(denied)[:200]}",
    )

    # --- refs are provisioned by the DevOps seat ------------------------------------------
    main = next(
        ref
        for ref in devops.call("GET", f"/repos/{repo}/refs")["refs"]
        if ref["name"] == DEFAULT_REF
    )
    devops.call(
        "POST",
        f"/repos/{repo}/refs",
        {"name": "lane/a", "target": main["target"], "reason": "role pilot provisioning"},
    )
    devops.check("refs: the DevOps seat provisioned lane/a", True)
    ref_status, ref_body = worker.status(
        "POST",
        f"/repos/{repo}/refs",
        {"name": "lane/worker", "target": main["target"], "reason": "worker attempt"},
    )
    worker.check(
        "refs: a worker cannot create a ref (403)",
        ref_status == 403,
        f"HTTP {ref_status} {str(ref_body)[:200]}",
    )

    # --- the worker checks in to its own lane ---------------------------------------------
    pushed = worker.call(
        "POST",
        f"/repos/{repo}/push",
        push_body(main["target"], "worker.txt", "worker check-in", "lane/a"),
    )
    worker.check(
        "check-in: the worker landed a changeset on its own lane",
        pushed["ref"]["name"] == "lane/a" and pushed["ref"]["version"] > 1,
        str(pushed.get("ref")),
    )
    head = pushed["ref"]["target"]

    # --- merge advance belongs to the DevOps seat -----------------------------------------
    advance_status, advance_body = worker.status(
        "POST",
        f"/repos/{repo}/push",
        push_body(head, "trunk.txt", "worker attempts trunk", "main"),
    )
    worker.check(
        "merge advance: a worker cannot advance main (403)",
        advance_status == 403,
        f"HTTP {advance_status} {str(advance_body)[:200]}",
    )
    landed = devops.call(
        "POST",
        f"/repos/{repo}/land",
        {"ref": "main", "head": head},
    )
    devops.check(
        "merge advance: the DevOps seat landed the worker's head on main",
        landed["ref"]["version"] > main["version"],
        str(landed.get("ref")),
    )
    file = worker.call(
        "GET", f"/repos/{repo}/file?changeset={landed['ref']['target']}&path=worker.txt"
    )
    worker.check(
        "merge advance: the worker's file is on main after the DevOps landing",
        base64.b64decode(file["data"]) == CONTENT_A,
        str(file.get("entry")),
    )

    # --- conflict escalation queue -------------------------------------------------------
    #
    # A worker that cannot resolve a conflict files it for the DevOps seat; the orchestrator
    # polls the open list and launches a DevOps agent per item.
    tip = next(
        ref
        for ref in worker.call("GET", f"/repos/{repo}/refs")["refs"]
        if ref["name"] == DEFAULT_REF
    )["target"]
    left = worker.call(
        "POST",
        f"/repos/{repo}/push",
        push_body(tip, "clash.txt", "left", DEFAULT_REF, mode="none"),
    )["changeset"]["id"]
    right = worker.call(
        "POST",
        f"/repos/{repo}/push",
        push_body(
            tip,
            "clash.txt",
            "right",
            DEFAULT_REF,
            blob=BLOB_B,
            content=CONTENT_B,
            mode="none",
        ),
    )["changeset"]["id"]
    merge_status, merge_error = worker.status(
        "POST",
        f"/repos/{repo}/merge",
        {
            "a": left,
            "b": right,
            "meta": {
                "author": "ForgeVCS role pilot <role@example.test>",
                "message": "conflicting merge",
            },
            "change_id": secrets.token_hex(16),
        },
    )
    worker.check(
        "queue: the conflicting merge is refused 409",
        merge_status == 409,
        f"HTTP {merge_status} {str(merge_error)[:200]}",
    )
    report = merge_error.get("details", {}).get("report", {})
    escalated = worker.call(
        "POST",
        f"/repos/{repo}/conflicts/escalate",
        {
            "ref": DEFAULT_REF,
            "head": right,
            "onto": left,
            "note": "add_add on clash.txt; worker cannot choose",
            "report": report,
        },
    )
    escalation_id = escalated["escalation"]["id"]
    worker.check(
        "queue: the worker escalated the conflict",
        bool(escalation_id) and escalated["escalation"]["status"] == "open",
        str(escalated)[:250],
    )
    denied_list, _ = worker.status("GET", f"/repos/{repo}/conflicts?status=open")
    worker.check(
        "queue: a worker cannot list the queue (403)",
        denied_list == 403,
        f"HTTP {denied_list}",
    )
    open_items = devops.call("GET", f"/repos/{repo}/conflicts?status=open")["escalations"]
    devops.check(
        "queue: DevOps sees the escalation",
        any(item["id"] == escalation_id for item in open_items),
        str(open_items)[:250],
    )
    denied_claim, _ = worker.status(
        "POST", f"/repos/{repo}/conflicts/{escalation_id}/claim"
    )
    worker.check(
        "queue: a worker cannot claim (403)",
        denied_claim == 403,
        f"HTTP {denied_claim}",
    )
    claimed = devops.call(
        "POST", f"/repos/{repo}/conflicts/{escalation_id}/claim"
    )["escalation"]
    devops.check(
        "queue: DevOps claimed it",
        claimed["status"] == "claimed" and claimed["claimed_by"],
        str(claimed)[:250],
    )
    resolved = devops.call(
        "POST",
        f"/repos/{repo}/conflicts/{escalation_id}/resolve",
        {"resolution": "DevOps resolved: keep theirs"},
    )["escalation"]
    devops.check(
        "queue: DevOps resolved it",
        resolved["status"] == "resolved" and resolved["resolution"],
        str(resolved)[:250],
    )
    after = devops.call("GET", f"/repos/{repo}/conflicts?status=open")["escalations"]
    devops.check(
        "queue: the resolved item is gone from the open list",
        all(item["id"] != escalation_id for item in after),
        str(after)[:250],
    )

    # --- basic level: read, merge-base, checkout ------------------------------------------
    worker.call("GET", f"/repos/{repo}/refs")
    worker.check("basic level: the worker can read refs", True)
    worker.call("GET", f"/repos/{repo}/merge-base?a={head}&b={main['target']}")
    worker.check("basic level: the worker can compute a merge base", True)
    workspace = worker.call("POST", f"/repos/{repo}/workspaces", {"ref": DEFAULT_REF})
    worker.check(
        "basic level: the worker can create a workspace (check-out)",
        bool(workspace.get("workspace_id")),
        str(workspace)[:200],
    )
    worker.call("DELETE", f"/repos/{repo}/workspaces/{workspace['workspace_id']}")

    # --- deletion: absent today, same seat when it exists ---------------------------------
    delete_status, _ = devops.status("DELETE", f"/repos/{repo}")
    print(
        f"INFO  deletion-today: DELETE /repos/{{repo}} is HTTP {delete_status}; "
        "no deletion endpoint yet — when built it must require admin.repos (DevOps)"
    )

    total = worker.checks + devops.checks
    print(f"OK  {total} checks passed; repository created: {repo}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RoleError as error:
        print(f"FAIL  {error}", file=sys.stderr)
        raise SystemExit(1)
