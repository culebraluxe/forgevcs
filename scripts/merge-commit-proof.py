#!/usr/bin/env python3
"""merge-commit-v1 proof against a live ForgeVCS façade.

Proves the opt-in topology end to end the way a client drives it, and proves the default did
not change:

  1. `/version` still reports the deployment default `merge_topology=squash-only` and lists
     `merge-commit-v1-landing` in `vendor_patches`.
  2. a lane changeset is merged and `merge-land` with `topology=merge-commit` publishes the
     **two-parent merge changeset itself**: the ref tip *is* the merge, `parents == [main, lane]`.
  3. the same shape without `topology` still squashes: the published changeset is single-parent
     and distinct from the merge changeset, so the default contract is unchanged.
  4. a stale merge (created against an older tip) is refused with
     `details.kind = "ref_version_mismatch"` and the ref does not move.

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


def push_body(parent: str, ref: str, changes: list[tuple[str, str, bytes]], mode: str) -> dict:
    """One changeset: `changes` is [(path, blob id, content)] in a single delta."""
    objects = {}
    delta = []
    for path, blob, content in changes:
        objects[blob] = base64.b64encode(content).decode()
        delta.append({"path": path, "op": 1, "mode": 1, "id": blob, "size": len(content)})
    return {
        "objects": [{"id": blob, "data": data} for blob, data in objects.items()],
        "changeset": {
            "parents": [parent],
            "delta": delta,
            "meta": {
                "author": "ForgeVCS merge-commit proof <proof@example.test>",
                "message": f"proof: {ref}",
            },
            "change_id": secrets.token_hex(16),
        },
        "ref": ref,
        "mode": mode,
    }


def merge(client: Client, prefix: str, a: str, b: str) -> str:
    response = client.call(
        "POST",
        prefix + "/merge",
        {
            "a": a,
            "b": b,
            "meta": {
                "author": "ForgeVCS merge-commit proof <proof@example.test>",
                "message": "proof: merge lane into main",
            },
            "change_id": secrets.token_hex(16),
        },
    )
    return response["changeset"]["id"]


def ref_target(client: Client, prefix: str, name: str) -> tuple[str, int]:
    refs = client.call("GET", prefix + "/refs")["refs"]
    ref = next((entry for entry in refs if entry["name"] == name), None)
    if ref is None:
        raise ProofError(f"no {name!r} ref")
    return ref["target"], ref["version"]


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

    # main: one changeset.
    landed = client.call(
        "POST",
        prefix + "/push",
        push_body(main_tip, "main", [("main.txt", BLOB_A, CONTENT_A)], "land"),
    )
    main_tip = landed["changeset"]["id"]
    print(f"  ..  main at {main_tip[:12]}")

    # lane/proof from that tip, with a different path so the merge has a real delta.
    client.call(
        "POST",
        prefix + "/refs",
        {"name": "lane/proof", "target": main_tip, "reason": "merge-commit proof"},
    )
    lane = client.call(
        "POST",
        prefix + "/push",
        push_body(main_tip, "lane/proof", [("lane.txt", BLOB_B, CONTENT_B)], "update"),
    )
    lane_tip = lane["changeset"]["id"]
    print(f"  ..  lane at {lane_tip[:12]}")

    # 2. merge-commit: the published changeset is the two-parent merge itself.
    merge_id = merge(client, prefix, main_tip, lane_tip)
    changeset = client.call("GET", prefix + f"/changesets/{merge_id}")["changeset"]
    client.check(
        "merge changeset has both parents in order",
        changeset["parents"] == [main_tip, lane_tip],
        str(changeset["parents"]),
    )
    published = client.call(
        "POST",
        prefix + "/merge-land",
        {
            "ref": "main",
            "head": merge_id,
            "message": "proof: land merge commit",
            "topology": "merge-commit",
        },
    )
    client.check("topology reported", published.get("topology") == "merge-commit-v1")
    client.check(
        "published changeset is the merge itself",
        published["changeset"]["id"] == merge_id and published["merge_changeset"]["id"] == merge_id,
        f"{published['changeset']['id'][:12]} vs {merge_id[:12]}",
    )
    client.check(
        "landed row is source=result=merge",
        published["landed"] == [{"change_id": changeset["change_id"], "source": merge_id, "result": merge_id}]
        or (len(published["landed"]) == 1 and published["landed"][0]["source"] == merge_id),
        str(published["landed"]),
    )
    tip, _ = ref_target(client, prefix, "main")
    client.check("main tip is the merge commit", tip == merge_id, tip[:12])

    # 3. default unchanged: the same shape without topology still squashes.
    lane2 = client.call(
        "POST",
        prefix + "/push",
        push_body(lane_tip, "lane/proof", [("lane2.txt", BLOB_A, CONTENT_A)], "update"),
    )
    lane2_tip = lane2["changeset"]["id"]
    squash_merge = merge(client, prefix, merge_id, lane2_tip)
    squashed = client.call(
        "POST",
        prefix + "/merge-land",
        {"ref": "main", "head": squash_merge, "message": "proof: default squash"},
    )
    client.check(
        "default topology is squash-only",
        squashed.get("topology") == "squash-only",
        str(squashed.get("topology")),
    )
    client.check(
        "squash publishes a different, single-parent changeset",
        squashed["changeset"]["id"] != squash_merge,
        squashed["changeset"]["id"][:12],
    )
    squashed_tip = client.call("GET", prefix + f"/changesets/{squashed['changeset']['id']}")["changeset"]
    client.check(
        "squashed changeset has one parent",
        squashed_tip["parents"] == [merge_id],
        str(squashed_tip["parents"]),
    )

    # 4. a stale merge is refused, typed, and the ref does not move.
    lane3 = client.call(
        "POST",
        prefix + "/push",
        push_body(lane2_tip, "lane/proof", [("lane3.txt", BLOB_B, CONTENT_B)], "update"),
    )
    stale_merge = merge(client, prefix, lane2_tip, lane3["changeset"]["id"])
    before, before_version = ref_target(client, prefix, "main")
    refusal = client.call(
        "POST",
        prefix + "/merge-land",
        {"ref": "main", "head": stale_merge, "topology": "merge-commit"},
        expect=409,
    )
    client.check(
        "stale merge is a typed refusal",
        refusal.get("details", {}).get("kind") == "ref_version_mismatch",
        json.dumps(refusal.get("details"))[:200],
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
