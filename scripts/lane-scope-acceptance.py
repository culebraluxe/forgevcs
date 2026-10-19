#!/usr/bin/env python3
"""lane-scope-acceptance.py — live proof that the narrowed lane policy (R3) really scopes paths.

Run through `scripts/lane-scope-acceptance.sh`, which boots ONE local service process against the
configured Neon branch with `scripts/pilot/acl-lanes.toml` and passes the generated credentials in
the environment. This client then proves, over HTTP with no mocks:

  ref provisioning   the DevOps seat can still create lane refs (`admin.refs` intact)
  lane/a allowed     agent-a lands a changeset under `lane/a/**` (agent-a is a repo writer too,
                     so the repository layer is satisfied for lane/a AND lane/b: the difference
                     below is the path policy, not the repository ACL)
  lane/b refused     the same agent is 403 on `lane/b/**` (per-agent lane scope)
  root refused       the same agent is 403 on `src/**` (root write removed from workers)
  lane/b allowed     agent-b can land under `lane/b/**` (the refusal above is scoping, not a
                     global deny)
  cross-repo refused agent-a is 403 reading AND writing the other repository (repo ACL)
  human CLI          `forgevcs auth mint/status/refs/log/push/pull/workspace …` still works and
                     can write a root path the agent was refused

The two repositories this run creates are deleted at the end (pass or fail). Audit rows
(`forgevcs_auth_decisions`) remain by design; nothing else is left behind.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import secrets
import subprocess
import sys
import time
import urllib.error
import urllib.request

LANE_BLOB = "b131f01a022b99ccd886f907c92891ffda405a87725884a33b660ab1f1b00ee173"
LANE_CONTENT = b"lane scope acceptance\n"
HUMAN_BLOB = "b10e86548036a6b330d7c2b63c0390c7659bc32e78ca6bd3baf731fd4bd651aa01"
HUMAN_CONTENT = b"human cli acceptance\n"
DEFAULT_REF = "main"
HUMAN_LOGIN = "culebraluxe"


class CheckError(RuntimeError):
    """A graded acceptance check failed."""


class Client:
    def __init__(self, base: str, token: str = "") -> None:
        self.base = base.rstrip("/")
        self.token = token
        self.checks = 0

    def _request(self, method: str, path: str, body=None):
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.base + path, data=data, method=method)
        if data is not None:
            request.add_header("content-type", "application/json")
        if self.token:
            request.add_header("authorization", "Bearer " + self.token)
        try:
            with urllib.request.urlopen(request, timeout=120) as response:
                return response.status, response.read()
        except urllib.error.HTTPError as error:
            return error.code, error.read()

    def call(self, method: str, path: str, body=None, expect: int = 200):
        status, raw = self._request(method, path, body)
        if status != expect:
            raise CheckError(f"{method} {path}: expected {expect}, got {status}: {raw[:500]!r}")
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
            raise CheckError(f"{name}: {detail}")
        self.checks += 1
        print(f"PASS  {name}")


def mint(base: str, principal: str, secret: str) -> str:
    token = Client(base).call(
        "POST", "/auth/token", {"service": principal, "secret": secret}
    ).get("token")
    if not token:
        raise CheckError(f"{principal}: no token minted")
    return token


def push_body(
    parent: str,
    path: str,
    message: str,
    ref: str,
    *,
    blob: str,
    content: bytes,
    mode: str = "land",
) -> dict:
    return {
        "objects": [{"id": blob, "data": base64.b64encode(content).decode()}],
        "changeset": {
            "parents": [parent],
            "delta": [{"path": path, "op": 1, "mode": 1, "id": blob, "size": len(content)}],
            "meta": {
                "author": "Lane scope acceptance <lane-scope@example.test>",
                "message": message,
            },
            "change_id": secrets.token_hex(16),
        },
        "ref": ref,
        "mode": mode,
    }


def denial_message(body: dict) -> str:
    """The façade's reason for a refusal (`message` in the error envelope)."""
    return str(body.get("message", ""))


def is_repository_layer_denial(body: dict) -> bool:
    """True when the refusal came from `forgevcs_repo_acl`, not the path policy."""
    message = denial_message(body)
    return (
        "cross-repository" in message or "repository ACL denied" in message
    ) and "Scion policy denied" not in message


def main_ref(refs: dict) -> dict:
    for ref in refs.get("refs", []):
        if ref.get("name") == DEFAULT_REF:
            return ref
    raise CheckError(f"repository has no {DEFAULT_REF!r} ref: {refs}")


class Cli:
    """The real `forgevcs` binary, invoked exactly as a human would."""

    def __init__(self, binary: str, base: str, config_dir: str) -> None:
        self.binary = binary
        self.base = base
        self.config_dir = config_dir
        self.env = os.environ.copy()
        # A developer shell may export a token for the hosted service; the acceptance must use
        # only the token this script mints locally.
        for variable in (
            "FORGEVCS_TOKEN",
            "FORGEVCS_BASE_URL",
            "FORGEVCS_CONFIG_DIR",
            "FORGEVCS_TOKEN_FILE",
        ):
            self.env.pop(variable, None)

    def run(self, *args: str) -> dict:
        completed = subprocess.run(
            [
                self.binary,
                "--base-url",
                self.base,
                "--config-dir",
                self.config_dir,
                *args,
            ],
            capture_output=True,
            text=True,
            timeout=180,
            env=self.env,
        )
        if completed.returncode != 0:
            raise CheckError(
                f"forgevcs {' '.join(args)}: exit {completed.returncode}: "
                f"{completed.stderr.strip()[:400]}"
            )
        try:
            return json.loads(completed.stdout)
        except json.JSONDecodeError as error:
            raise CheckError(f"forgevcs {' '.join(args)}: non-JSON output: {error}") from error


def human_cli_checks(args, repo: str) -> None:
    """Prove the human path over the narrowed policy, using the shipped binary end to end."""
    human_secret_file = os.environ["LANE_HUMAN_SECRET_FILE"]
    cli = Cli(args.cli, args.base, args.config_dir)

    minted = cli.run("auth", "mint", "--login", HUMAN_LOGIN, "--secret-file", human_secret_file)
    if minted.get("principal") != f"github:{HUMAN_LOGIN}":
        raise CheckError(f"human mint principal: {minted}")
    if "token" in minted:
        raise CheckError("auth mint printed the bearer token without --show-token")
    print("PASS  human CLI: auth mint stores a token without printing it")

    status = cli.run("auth", "status")
    if status.get("token_present") is not True or status.get("token_source") != "file":
        raise CheckError(f"auth status: {status}")
    if status.get("whoami", {}).get("principal") != f"github:{HUMAN_LOGIN}":
        raise CheckError(f"auth status whoami: {status.get('whoami')}")
    print("PASS  human CLI: auth status verifies github:culebraluxe")

    refs = cli.run("refs", repo)
    main = main_ref(refs)
    print("PASS  human CLI: refs")

    log = cli.run("log", repo, "--ref", DEFAULT_REF, "--limit", "3")
    if not log.get("commits"):
        raise CheckError(f"log returned no commits: {log}")
    print("PASS  human CLI: log")

    # Human devops seat: ref provisioning still works through the CLI after the narrowing.
    cli.run(
        "ref",
        "create",
        repo,
        "--name",
        "lane/human",
        "--target",
        main["target"],
        "--reason",
        "human devops seat",
    )
    if not any(ref.get("name") == "lane/human" for ref in cli.run("refs", repo).get("refs", [])):
        raise CheckError("human ref create did not appear in refs")
    print("PASS  human CLI: ref create (admin.refs intact for the human devops seat)")

    # A root path the worker was refused above: only the DevOps/human grant can write it.
    body = push_body(main["target"], "src/human-acceptance.txt", "human root write", DEFAULT_REF,
                     blob=HUMAN_BLOB, content=HUMAN_CONTENT, mode="none")
    body_path = os.path.join(args.config_dir, "human-push.json")
    with open(body_path, "w", encoding="utf-8") as handle:
        json.dump(body, handle)
    pushed = cli.run("push", repo, "--changeset", body_path)
    if not pushed.get("changeset", {}).get("id"):
        raise CheckError(f"human push: {pushed}")
    print("PASS  human CLI: push under src/ (root write; the worker was refused)")

    pulled = cli.run("pull", repo, "--ref", DEFAULT_REF, "--objects", HUMAN_BLOB)
    if not any(item.get("status") == "ok" for item in pulled.get("objects", [])):
        raise CheckError(f"pull object: {pulled}")
    print("PASS  human CLI: pull object bytes")

    workspace = cli.run("workspace", "create", repo, "--ref", DEFAULT_REF)
    workspace_id = workspace.get("workspace_id")
    if not workspace_id:
        raise CheckError(f"workspace create: {workspace}")
    cli.run("workspace", "status", repo, workspace_id)
    cli.run("workspace", "destroy", repo, workspace_id)
    print("PASS  human CLI: workspace create/status/destroy")


def run(args) -> int:
    base = args.base
    repo_a, repo_b = args.repo_a, args.repo_b

    secrets_by_name = {
        "agent-a": os.environ["LANE_AGENT_A_SECRET"],
        "agent-b": os.environ["LANE_AGENT_B_SECRET"],
        "devops": os.environ["LANE_DEVOPS_SECRET"],
    }
    tokens = {name: mint(base, name, secret) for name, secret in secrets_by_name.items()}
    agent_a = Client(base, tokens["agent-a"])
    agent_b = Client(base, tokens["agent-b"])
    devops = Client(base, tokens["devops"])

    print(f"INFO  engine {base}")
    print(f"INFO  repos  {repo_a}, {repo_b}")

    created_a = devops.call("POST", f"/repos/{repo_a}/init", {})
    created_b = devops.call("POST", f"/repos/{repo_b}/init", {})
    if not created_a.get("created") or not created_b.get("created"):
        raise CheckError(f"init did not create fresh repos: {created_a} {created_b}")
    main = main_ref(devops.call("GET", f"/repos/{repo_a}/refs"))
    print("PASS  devops: created the two acceptance repositories")

    # `admin.refs` must survive the path narrowing: provision both lane refs at main.
    for lane in ("lane/a", "lane/b"):
        devops.call(
            "POST",
            f"/repos/{repo_a}/refs",
            {"name": lane, "target": main["target"], "reason": "lane scope acceptance"},
        )
    devops.check("devops: ref create still works (admin.refs intact)", True)

    who = agent_a.call("GET", "/auth/whoami")
    agent_a.check("identity: /auth/whoami names agent-a", "agent-a" in who.get("principal", ""), str(who))

    allowed = agent_a.call(
        "POST",
        f"/repos/{repo_a}/push",
        push_body(main["target"], "lane/a/agent-a.txt", "agent-a lane write", "lane/a",
                  blob=LANE_BLOB, content=LANE_CONTENT),
    )
    agent_a.check(
        "path scope: agent-a push under lane/a is allowed",
        bool(allowed.get("changeset", {}).get("id"))
        and allowed.get("ref", {}).get("name") == "lane/a",
        str(allowed),
    )

    lane_b_status, lane_b_body = agent_a.status(
        "POST",
        f"/repos/{repo_a}/push",
        push_body(main["target"], "lane/b/agent-a.txt", "agent-a lane/b write", "lane/b",
                  blob=LANE_BLOB, content=LANE_CONTENT),
    )
    # Same repository, same writer grant in `forgevcs_repo_acl`: only the PATH policy differs
    # between lane/a and lane/b, and the refusal message must be the Scion policy layer (not the
    # repository layer) or this would not prove path scoping.
    agent_a.check(
        "path scope: agent-a push under lane/b is refused (403, path policy)",
        lane_b_status == 403 and "Scion policy denied" in denial_message(lane_b_body),
        f"HTTP {lane_b_status} {str(lane_b_body)[:200]}",
    )

    root_status, root_body = agent_a.status(
        "POST",
        f"/repos/{repo_a}/push",
        push_body(main["target"], "src/agent-a.txt", "agent-a root write", DEFAULT_REF,
                  blob=LANE_BLOB, content=LANE_CONTENT, mode="none"),
    )
    agent_a.check(
        "path scope: agent-a push under src/ is refused (403, path policy)",
        root_status == 403 and "Scion policy denied" in denial_message(root_body),
        f"HTTP {root_status} {str(root_body)[:200]}",
    )

    # The lane/b refusal above is scoping, not a global deny: agent-b owns lane/b.
    agent_b_allowed = agent_b.call(
        "POST",
        f"/repos/{repo_a}/push",
        push_body(main["target"], "lane/b/agent-b.txt", "agent-b lane write", "lane/b",
                  blob=LANE_BLOB, content=LANE_CONTENT),
    )
    agent_b.check(
        "path scope: agent-b push under lane/b is allowed",
        bool(agent_b_allowed.get("changeset", {}).get("id")),
        str(agent_b_allowed),
    )

    cross_read_status, cross_read = agent_a.status("GET", f"/repos/{repo_b}/refs")
    agent_a.check(
        "repository scope: agent-a read of the other repository is refused (403, repo ACL)",
        cross_read_status == 403 and is_repository_layer_denial(cross_read),
        f"HTTP {cross_read_status} {str(cross_read)[:200]}",
    )
    cross_write_status, cross_write = agent_a.status(
        "POST",
        f"/repos/{repo_b}/push",
        push_body(main["target"], "lane/a/agent-a.txt", "cross-repo write", "lane/a",
                  blob=LANE_BLOB, content=LANE_CONTENT, mode="none"),
    )
    agent_a.check(
        "repository scope: agent-a write to the other repository is refused (403, repo ACL)",
        cross_write_status == 403 and is_repository_layer_denial(cross_write),
        f"HTTP {cross_write_status} {str(cross_write)[:200]}",
    )

    agent_a.call("GET", f"/repos/{repo_a}/refs")
    agent_a.check("repository scope: agent-a can read its own repository", True)

    human_cli_checks(args, repo_a)

    return 0


def teardown(devops: Client, repos: list[str]) -> None:
    for repo in repos:
        try:
            devops.call("DELETE", f"/repos/{repo}", {"confirm": repo})
            print(f"TEARDOWN  deleted {repo}")
        except Exception as error:  # noqa: BLE001 - teardown is best effort, never graded
            print(f"TEARDOWN  could not delete {repo}: {error}", file=sys.stderr)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="http://127.0.0.1:4132")
    parser.add_argument("--repo-a", required=True)
    parser.add_argument("--repo-b", required=True)
    parser.add_argument("--cli", required=True, help="path to the forgevcs binary")
    parser.add_argument("--config-dir", required=True, help="temporary CLI config directory")
    args = parser.parse_args()

    repos = [args.repo_a, args.repo_b]
    devops_token = ""
    exit_code = 1
    started = time.monotonic()
    try:
        # A devops client for teardown, even if run() fails before building its own.
        devops_token = mint(args.base, "devops", os.environ["LANE_DEVOPS_SECRET"])
        exit_code = run(args)
    except (CheckError, KeyError, urllib.error.URLError) as error:
        print(f"FAIL  {error}", file=sys.stderr)
    finally:
        if devops_token:
            teardown(Client(args.base, devops_token), repos)
        elapsed = time.monotonic() - started
        verdict = "PASS" if exit_code == 0 else "FAIL"
        print(f"LANE_SCOPE={verdict} elapsed={elapsed:.1f}s")
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
