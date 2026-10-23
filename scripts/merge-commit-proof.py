#!/usr/bin/env python3
"""merge-commit-v1 proof against a live ForgeVCS façade.

Proves the opt-in topology end to end the way a client drives it, and proves the default did
not change:

  1. `/version` still reports the deployment default `merge_topology=squash-only` and lists
     `merge-commit-v1-landing` in `vendor_patches`.
  2. `merge-land` with `topology=merge-commit` publishes the **two-parent merge changeset
     itself**: the ref tip *is* the merge and `parents == [main, lane]`.
  3. the same call without `topology` still squashes: the published changeset is single-parent
     and distinct from the merge changeset, so the default contract is unchanged.
  4. re-landing a lane tip whose content is already merged is refused as an empty merge
     (`409`), and the ref does not move.

Staleness (`REF_VERSION_MISMATCH` for a merge whose first parent is no longer the tip) is an
engine-level guarantee: the façade re-runs `Merge(tip, head)` inside the request, so a stale
merge cannot be handed to it deterministically. That path is covered by
`upstream/crates/scion-server/tests/write_land.rs` (`land_merge_stale_first_parent_is_a_typed_refusal`).

Usage:
    python3 scripts/merge-commit-proof.py --base https://… --token <bearer> --repo <name>

The repo is created by the run (V0 has no repository deletion), so pass a fresh name per run.
No third-party packages: only the standard library.
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


class ProofError(RuntimeError):
    pass


class Client:
    def __init__(self, base: str, token: str) -> None:
        self.base = base.rstrip("/")
        self.token = token
        self.checks = 0

    def call(self, method: str, path: str, body=None, expect: int = 200):
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(self.base + path, data=data, method=method)
        if data is not None:
            request.add_header("content-type", "application/json")
        request.add_header("authorization", f"Bearer {self.token}")
        try:
            with urllib.request.urlopen(request, timeout=120) as response:
                status, raw = response.status, response.read()
        except urllib.error.HTTPError as error:
            status, raw = error.code, error.read()
        if status != expect:
            raise ProofError(f"{method} {path} -> HTTP {status} (expected {expect}): {raw[:400]!r}")
        return json.loads(raw) if raw else {}

    def check(self, name: str, condition: bool, detail: str = "") -> None:
        self.checks += 1
        if not condition:
            raise ProofError(f"FAILED: {name}{(' — ' + detail) if detail else ''}")
        print(f"  ok  {name}")


def push_body(parent: str, ref: str, path: str, blob: str, content: bytes, mode: str) -> dict:
    return {
        "objects": [{"id": blob, "data": base64.b64encode(content).decode()}],
        "changeset": {
            "parents": [parent],
            "delta": [{"path": path, "op": 1, "mode": 1, "id": blob, "size": len(content)}],
            "meta": {
                "author": "ForgeVCS merge-commit proof <proof@example.test>",
                "message": f"proof: {path}",
            },
            "change_id": secrets.token_hex(16),
        },
        "ref": ref,
        "mode": mode,
    }


def ref_target(client: Client, prefix: str, name: str) -> tuple[str, int]:
    refs = client.call("GET", prefix + "/refs")["refs"]
    ref = next((entry for entry in refs if entry["name"] == name), None)
    if ref is None:
        raise ProofError(f"no {name!r} ref")
    return ref["target"], ref["version"]


def changeset(client: Client, prefix: str, id: str) -> dict:
    return client.call("GET", prefix + f"/changesets/{id}")["changeset"]


def run(base: str, token: str, repo: str) -> int:
    client = Client(base, token)
    prefix = f"/repos/{repo}"

    print(f"== {base}  repo {repo}")
    version = client.call("GET", "/version")
    client.check(
        "vendor patch is deployed",
        "merge-commit-v1-landing" in version.get("vendor_patches", []),
        str(version.get("vendor_patches")),
    )
    client.check(
        "deployment default is still squash-only",
        version.get("merge_topology") == "squash-only",
        str(version.get("merge_topology")),
    )

    client.call("POST", prefix + "/init", {})
    main_tip, _ = ref_target(client, prefix, "main")

    landed = client.call(
        "POST", prefix + "/push", push_body(main_tip, "main", "main.txt", BLOB_A, CONTENT_A, "land")
    )
    main_tip = landed["changeset"]["id"]
    print(f"  ..  main at {main_tip[:12]}")

    lane = client.call(
        "POST",
        prefix + "/push",
        push_body(main_tip, "lane/proof", "lane.txt", BLOB_B, CONTENT_B, "none"),
    )
    lane_tip = lane["changeset"]["id"]
    print(f"  ..  lane at {lane_tip[:12]}")
    # This run authenticates as the DevOps seat, which the policy denies on `lane/*` (correctly:
    # only `group:agents` advances lane refs). The changeset is stored with mode `none` and the
    # ref is provisioned through `admin.refs`, so the topology is proven without touching policy.
    client.call(
        "POST",
        prefix + "/refs",
        {"name": "lane/proof", "target": lane_tip, "reason": "merge-commit proof"},
    )
    provisioned, _ = ref_target(client, prefix, "lane/proof")
    client.check("lane ref holds the lane changeset", provisioned == lane_tip, provisioned[:12])

    # 1. merge-commit: the endpoint mints Merge(main, lane) and publishes it as itself.
    published = client.call(
        "POST",
        prefix + "/merge-land",
        {
            "ref": "main",
            "head": lane_tip,
            "message": "proof: land merge commit",
            "topology": "merge-commit",
        },
    )
    merge_id = published["changeset"]["id"]
    client.check("topology reported", published.get("topology") == "merge-commit-v1")
    client.check(
        "published changeset is the merge itself",
        published["changeset"]["id"] == published["merge_changeset"]["id"],
        f"{published['changeset']['id'][:12]} vs {published['merge_changeset']['id'][:12]}",
    )
    merge = changeset(client, prefix, merge_id)
    client.check(
        "merge changeset has both parents in order",
        merge["parents"] == [main_tip, lane_tip],
        str(merge["parents"]),
    )
    client.check(
        "landed row is source=result=merge",
        len(published["landed"]) == 1
        and published["landed"][0]["source"] == merge_id
        and published["landed"][0]["result"] == merge_id,
        str(published["landed"]),
    )
    tip, _ = ref_target(client, prefix, "main")
    client.check("main tip is the merge commit", tip == merge_id, tip[:12])

    # 2. default unchanged: the same call without topology still squashes.
    lane2 = client.call(
        "POST",
        prefix + "/push",
        push_body(lane_tip, "lane/proof", "lane2.txt", BLOB_A, CONTENT_A, "none"),
    )
    lane2_tip = lane2["changeset"]["id"]
    squashed = client.call(
        "POST",
        prefix + "/merge-land",
        {"ref": "main", "head": lane2_tip, "message": "proof: default squash"},
    )
    client.check(
        "default topology is squash-only",
        squashed.get("topology") == "squash-only",
        str(squashed.get("topology")),
    )
    client.check(
        "squash publishes a different changeset than the merge",
        squashed["changeset"]["id"] != squashed["merge_changeset"]["id"],
        squashed["changeset"]["id"][:12],
    )
    squashed_changeset = changeset(client, prefix, squashed["changeset"]["id"])
    client.check(
        "squashed changeset has exactly one parent (the previous tip)",
        squashed_changeset["parents"] == [merge_id],
        str(squashed_changeset["parents"]),
    )

    # 3. an already-merged lane tip is refused as an empty merge, and the ref stays put.
    before, before_version = ref_target(client, prefix, "main")
    refusal = client.call(
        "POST",
        prefix + "/merge-land",
        {"ref": "main", "head": lane2_tip, "topology": "merge-commit"},
        expect=409,
    )
    client.check(
        "re-merging a merged lane is refused",
        "empty delta" in json.dumps(refusal),
        json.dumps(refusal)[:200],
    )
    after, after_version = ref_target(client, prefix, "main")
    client.check(
        "ref did not move on refusal",
        (after, after_version) == (before, before_version),
        f"{after[:12]}@{after_version}",
    )

    print(f"OK  {client.checks} checks passed for {repo}")
    return client.checks


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True, help="façade base URL")
    parser.add_argument("--token", required=True, help="bearer token")
    parser.add_argument("--repo", required=True, help="repository name to create")
    args = parser.parse_args()
    try:
        run(args.base, args.token, args.repo)
    except ProofError as error:
        print(f"FAILED: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
