#!/usr/bin/env python3
"""agent-pilot.py — three service principals against one ForgeVCS engine.

This is the M1 multi-agent pilot client. It drives the REST facade exactly as three
independent bots would: each principal mints its own token, writes its own repository,
and races the others on a shared repository's ref.

What it proves, in order:

  identity     every principal mints a token and /auth/whoami names it
  isolation    agent-a can read agent-b's repository (RECORDED, not failed): under the
               shipped acl.toml every agent is in one `agents` group, so this is the
               known per-repository ACL gap, not a pilot regression
  independent  three agents push three changesets each, concurrently, to three repositories
  shared ref   three agents push DISJOINT paths to one shared ref concurrently: all three
               land, because Land rebases a contended changeset onto the new tip instead
               of rejecting it; the final tree carries all three files
  contention   two agents push the SAME path to that ref concurrently: exactly one lands,
               the other gets a clear 409; the ref is not lost and nothing is half-applied
  recovery     the loser re-reads the ref and lands a changeset on the winner's tip
  workspaces   three agents each create a workspace, write, and submit concurrently

It never invents identities: the principals and secrets come from
FORGEVCS_SERVICE_PRINCIPALS, which scripts/agent-pilot.sh sets before booting the engine.

There is no repository deletion endpoint yet (docs/M1-readiness-and-multi-agent.md), so
this client creates four repositories and prints their names for manual cleanup.

Exit 0 only when every graded check passes. No third-party Python packages are used.
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import json
import secrets
import sys
import threading
import time
import urllib.error
import urllib.request

# BLAKE3-256 of the two fixed payloads, with Scion's `b1` kind prefix. The service verifies every
# upload against its id, so a mismatch fails the run rather than being papered over.
BLOB_A = "b1d3bae0cd810e6f0ca2bdfa4a431f5810e4c1878c0ada9ffcddc706c25020d430"
BLOB_B = "b175ae44830c60dd21712afd5fcf09a4bf86d98d8b3cec75c7b5c0681b8ffb12e9"
CONTENT_A = b"ForgeSC smoke test\n"
CONTENT_B = b"ForgeVCS smoke test\n"
B64_A = base64.b64encode(CONTENT_A).decode()
B64_B = base64.b64encode(CONTENT_B).decode()

DEFAULT_REF = "main"


class PilotError(RuntimeError):
    """A graded check failed."""


class Client:
    """One HTTP client. A Client carries a token; it is safe to build one per thread."""

    def __init__(self, base: str, token: str = "") -> None:
        self.base = base.rstrip("/")
        self.token = token
        self.checks = 0

    def _request(
        self,
        method: str,
        path: str,
        body=None,
        token: bool = True,
        timeout: int = 120,
    ):
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.base + path, data=data, method=method)
        if data is not None:
            request.add_header("content-type", "application/json")
        if token and self.token:
            request.add_header("authorization", "Bearer " + self.token)
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                return response.status, response.read()
        except urllib.error.HTTPError as error:
            return error.code, error.read()

    def call(self, method: str, path: str, body=None, token: bool = True, expect: int = 200):
        status, raw = self._request(method, path, body, token)
        if status != expect:
            raise PilotError(
                f"{method} {path}: expected HTTP {expect}, got {status}: {raw[:600]!r}"
            )
        return json.loads(raw) if raw else {}

    def status(self, method: str, path: str, body=None, token: bool = True):
        status, raw = self._request(method, path, body, token)
        try:
            parsed = json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            parsed = {"raw": raw.decode("utf-8", "replace")[:600]}
        return status, parsed

    def check(self, name: str, condition: bool, detail: str = "") -> None:
        if not condition:
            raise PilotError(f"{name}: {detail}")
        self.checks += 1
        print(f"PASS  {name}")


def mint(base: str, principal: str, secret: str) -> str:
    client = Client(base)
    token = client.call(
        "POST", "/auth/token", {"service": principal, "secret": secret}
    ).get("token")
    if not token:
        raise PilotError(f"{principal}: no token minted")
    return token


def push_body(parent: str, path: str, blob: str, size: int, data: str, message: str) -> dict:
    return {
        "objects": [{"id": blob, "data": data}],
        "changeset": {
            "parents": [parent],
            "delta": [{"path": path, "op": 1, "mode": 1, "id": blob, "size": size}],
            "meta": {
                "author": "ForgeVCS agent pilot <pilot@example.test>",
                "message": message,
            },
            "change_id": secrets.token_hex(16),
        },
        "ref": DEFAULT_REF,
        "mode": "land",
    }


def init_repo(client: Client, repo: str) -> tuple[str, int]:
    client.call("POST", f"/repos/{repo}/init", {})
    refs = client.call("GET", f"/repos/{repo}/refs")
    main = next(ref for ref in refs["refs"] if ref["name"] == DEFAULT_REF)
    return main["target"], main["version"]


def main_target(client: Client, repo: str) -> tuple[str, int]:
    refs = client.call("GET", f"/repos/{repo}/refs")
    main = next(ref for ref in refs["refs"] if ref["name"] == DEFAULT_REF)
    return main["target"], main["version"]


def land(client: Client, repo: str, parent: str, path: str, blob: str, size: int, data: str, message: str):
    """Push one changeset and return (changeset id, ref target, ref version)."""
    pushed = client.call(
        "POST",
        f"/repos/{repo}/push",
        push_body(parent, path, blob, size, data, message),
    )
    return (
        pushed["changeset"]["id"],
        pushed["ref"]["target"],
        pushed["ref"]["version"],
    )


def tree_paths(client: Client, repo: str, changeset: str) -> set[str]:
    tree = client.call("GET", f"/repos/{repo}/tree?changeset={changeset}")
    return {entry["name"] for entry in tree["entries"]}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="http://127.0.0.1:3998")
    parser.add_argument(
        "--agents",
        required=True,
        help="comma-separated principal=secret list, e.g. agent-a=one,agent-b=two",
    )
    args = parser.parse_args()

    agents: list[tuple[str, str]] = []
    for item in args.agents.split(","):
        principal, _, secret = item.partition("=")
        if not principal or not secret:
            print(f"FAIL  malformed --agents entry: {item}", file=sys.stderr)
            return 2
        agents.append((principal.strip(), secret.strip()))
    if len(agents) != 3:
        print("FAIL  this pilot expects exactly three agents", file=sys.stderr)
        return 2

    stamp = time.strftime("%y%m%d%H%M%S") + secrets.token_hex(2)
    repos = {principal: f"agent-pilot-{stamp}-{principal.split('-')[-1]}" for principal, _ in agents}
    shared_repo = f"agent-pilot-{stamp}-shared"
    created = [*repos.values(), shared_repo]

    base = args.base
    tokens = {principal: mint(base, principal, secret) for principal, secret in agents}
    clients = {principal: Client(base, token) for principal, token in tokens.items()}

    print(f"INFO  engine {base}")
    print(f"INFO  agents {', '.join(tokens)}")
    print(f"INFO  stamp  {stamp}")

    # --- identity -------------------------------------------------------------------------
    for principal, client in clients.items():
        who = client.call("GET", "/auth/whoami")
        client.check(
            f"identity: /auth/whoami names {principal}",
            principal in who.get("principal", ""),
            str(who),
        )

    # --- one repository per agent --------------------------------------------------------
    for principal, repo in repos.items():
        init_repo(clients[principal], repo)

    # --- isolation today (recorded, not graded) -------------------------------------------
    first, second = agents[0][0], agents[1][0]
    status, _ = clients[first].status("GET", f"/repos/{repos[second]}/refs")
    print(
        f"INFO  isolation-today: {first} read {second}'s repo refs "
        f"(HTTP {status}) — expected under the shared `agents` group; "
        "per-repository ACL is the M1 P0 gap"
    )

    # --- independent repositories, concurrently ------------------------------------------
    before = {principal: main_target(clients[principal], repo)[1] for principal, repo in repos.items()}

    def independent_chain(index: int) -> None:
        principal = agents[index][0]
        client = clients[principal]
        repo = repos[principal]
        target, _ = main_target(client, repo)
        for step in range(3):
            path = f"{principal}-{step}.txt"
            target, _, _ = land(
                client,
                repo,
                target,
                path,
                BLOB_A,
                len(CONTENT_A),
                B64_A,
                f"{principal} independent {step}",
            )
            if not target:
                raise PilotError(f"{principal}: no ref target after independent push {step}")

    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
        list(pool.map(independent_chain, range(3)))
    for principal, repo in repos.items():
        target, version = main_target(clients[principal], repo)
        clients[principal].check(
            f"independent: {principal} advanced {repo} by 3",
            version == before[principal] + 3,
            f"version {before[principal]} -> {version}",
        )
        clients[principal].check(
            f"independent: {principal} tree carries all three paths",
            {f"{principal}-{step}.txt" for step in range(3)}
            <= tree_paths(clients[principal], repo, target),
            f"tip {target}",
        )

    # --- one shared ref: disjoint paths must all land ------------------------------------
    init_repo(clients[first], shared_repo)
    parent, version = main_target(clients[first], shared_repo)
    barrier = threading.Barrier(3)

    def shared_disjoint(index: int) -> tuple[str, int]:
        principal = agents[index][0]
        barrier.wait(timeout=60)
        _, target, landed_version = land(
            clients[principal],
            shared_repo,
            parent,
            f"{principal}.txt",
            BLOB_A,
            len(CONTENT_A),
            B64_A,
            f"{principal} shared disjoint",
        )
        return target, landed_version

    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
        results = list(pool.map(shared_disjoint, range(3)))
    tip, final_version = main_target(clients[first], shared_repo)
    paths = tree_paths(clients[first], shared_repo, tip)
    clients[first].check(
        "shared-ref: all three disjoint pushes landed",
        len(results) == 3,
        str(results),
    )
    clients[first].check(
        "shared-ref: final tree carries every agent's file",
        {f"{principal}.txt" for principal, _ in agents} <= paths,
        f"tip {tip} paths {sorted(paths)}",
    )
    clients[first].check(
        "shared-ref: the ref advanced once per landing",
        final_version >= version + 3,
        f"version {version} -> {final_version}",
    )

    # --- one shared ref: the same path must have exactly one winner -----------------------
    parent, version = main_target(clients[first], shared_repo)
    barrier = threading.Barrier(2)

    def shared_collision(index: int, content, blob: str, data: str):
        principal = agents[index][0]
        barrier.wait(timeout=60)
        return clients[principal].status(
            "POST",
            f"/repos/{shared_repo}/push",
            push_body(parent, "collision.txt", blob, len(content), data, f"{principal} collision"),
        )

    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        collision = list(
            pool.map(
                lambda index: shared_collision(
                    index,
                    CONTENT_A if index == 0 else CONTENT_B,
                    BLOB_A if index == 0 else BLOB_B,
                    B64_A if index == 0 else B64_B,
                ),
                range(2),
            )
        )
    statuses = sorted(status for status, _ in collision)
    clients[first].check(
        "contention: exactly one same-path push landed",
        statuses.count(200) == 1,
        f"statuses {statuses}",
    )
    clients[first].check(
        "contention: the other was a clear 409, not a lost ref",
        statuses.count(409) == 1,
        f"statuses {statuses}",
    )
    winner_index = next(index for index, (status, _) in enumerate(collision) if status == 200)
    loser_index = 1 - winner_index
    tip, version = main_target(clients[first], shared_repo)
    winner_content = CONTENT_A if winner_index == 0 else CONTENT_B
    file_at = clients[first].call(
        "GET", f"/repos/{shared_repo}/file?changeset={tip}&path=collision.txt"
    )
    clients[first].check(
        "contention: the ref carries the winner's bytes",
        base64.b64decode(file_at["data"]) == winner_content,
        str(file_at.get("entry")),
    )

    # --- recovery: the loser lands on the winner's tip -----------------------------------
    loser = agents[loser_index][0]
    winner = agents[winner_index][0]
    loser_content = CONTENT_A if loser_index == 0 else CONTENT_B
    loser_blob = BLOB_A if loser_index == 0 else BLOB_B
    loser_data = B64_A if loser_index == 0 else B64_B
    recovered = clients[loser].call(
        "POST",
        f"/repos/{shared_repo}/push",
        push_body(tip, "collision.txt", loser_blob, len(loser_content), loser_data, f"{loser} recovery"),
    )
    clients[loser].check(
        "recovery: the conflicted agent landed on the winner's tip",
        recovered["ref"]["version"] > version,
        str(recovered.get("ref")),
    )
    new_tip, _ = main_target(clients[first], shared_repo)
    now = clients[first].call(
        "GET", f"/repos/{shared_repo}/file?changeset={new_tip}&path=collision.txt"
    )
    clients[loser].check(
        "recovery: the loser's content is now the ref content",
        base64.b64decode(now["data"]) == loser_content,
        f"winner {winner} -> loser {loser}",
    )

    # --- workspaces, concurrently, one per agent -----------------------------------------
    def workspace_round(index: int) -> str:
        principal = agents[index][0]
        client = clients[principal]
        repo = repos[principal]
        created = client.call("POST", f"/repos/{repo}/workspaces", {"ref": DEFAULT_REF})
        workspace = created["workspace_id"]
        path = f"workspace-{principal}.txt"
        client.call(
            "PUT",
            f"/repos/{repo}/workspaces/{workspace}/file?path={path}",
            {"data": base64.b64encode(CONTENT_A).decode(), "mode": 1},
        )
        submitted = client.call(
            "POST",
            f"/repos/{repo}/workspaces/{workspace}/submit",
            {"message": f"{principal} workspace submit", "ref": DEFAULT_REF, "mode": "land"},
        )["workspace"]
        client.call("DELETE", f"/repos/{repo}/workspaces/{workspace}")
        return submitted["changeset"]["changeset"]

    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
        workspace_changesets = list(pool.map(workspace_round, range(3)))
    for index, changeset in enumerate(workspace_changesets):
        principal = agents[index][0]
        clients[principal].check(
            f"workspaces: {principal} submitted a changeset",
            bool(changeset),
            f"changeset {changeset}",
        )

    total = sum(client.checks for client in clients.values())
    print(f"OK  {total} checks passed across 3 agents and {len(created)} repositories")
    print("INFO  repositories created (no deletion endpoint yet): " + ", ".join(created))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except PilotError as error:
        print(f"FAIL  {error}", file=sys.stderr)
        raise SystemExit(1)
