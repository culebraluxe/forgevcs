# Merge-commit v1 topology (opt-in)

Status: **implemented as an opt-in alternate path** (Lane Z, 2026-10-22). The default remains
**squash-only**; nothing about the existing squash `merge-land` path changes. This document is the
contract for the opt-in topology; `docs/TOPOLOGY-DECISION.md` is the decision memo that deferred it
and is now superseded for mode B.

## Why

`merge-land` historically squashes: it builds the two-parent merge changeset `Merge(tip, head)` for
its tree/lineage, then creates a **single-parent** changeset from the merge delta and lands that
(`facade::merge_land`). The published trunk is linear and the merge changeset is lineage metadata.
Some teams want the merge commit itself on the trunk. This topology publishes it **as itself** for
clean, tip-parented merges only.

## The contract

- A merge changeset is `M = Merge(tip, head)` with `M.parents == [tip, head]`.
- `land_merge(M)` publishes `M` **only if `M.parents[0] == current ref tip`**. The ref CAS then
  moves `tip -> M` at the expected version.
- **No rebase, no force, no rewrite.** If the ref moved since the merge was created, the land is
  refused with a typed stale error carrying the current target/version; the caller must re-merge
  (create a new `Merge(new_tip, head)`) and land again.
- **Conflicts are refused explicitly.** If `Merge(tip, head)` conflicts, the merge is never
  created; the request returns `MERGE_CONFLICT` (the normal filtered conflict payload, with
  `"topology": "merge-commit-v1"` added at the façade) and **nothing is paused or landed** — no
  paused-then-continue linear resolution, no silent squash.
- **Policy is the land policy**: `update` on the ref (re-read every attempt) and `write` on every
  delta path of `M` (the merge delta is relative to `tip`, so these are exactly the published
  paths). Merge-aware conflict filtering is preserved (writable-but-unreadable conflicts fail
  closed, TD-12 patch).
- **Empty merges are refused**: a merge whose delta is empty changes no tree relative to the tip
  (its side is already contained in the ref, e.g. re-merging an already-merged lane). Publishing
  it would add a no-op merge commit and look like a second landing, so it fails `EMPTY_CHANGE`
  (HTTP `409 conflict` in the façade envelope) and the ref does not move.
- **One `landed` row**: `change_id(M) -> {source: M, result: M}` on `(ref, change_id)`. The
  changeset is published unchanged, so source and result are the same object.
- **Idempotent retry (engine level)**: if the CAS committed and the response was lost, retrying
  the *same merge changeset* finds the landed row and returns the recorded result against the
  current ref without a second ref-log row.
- **HTTP retry caveat**: `merge-land` creates the merge changeset itself, and its meta carries the
  server time. A client that retries the same request with the same `change_id` therefore asks for
  a *different* changeset with the same change id; the engine refuses it as `ALREADY_LANDED`
  (`409`, `details.kind = "already_landed"`, source = the landed merge) and the ref stays put.
  That is a safe refusal, not a double-land; true idempotent replay is the engine-level one above.
  Do not treat `ALREADY_LANDED` as failure to converge on a retry — re-read the ref, the merge is
  already published.
- **`ALREADY_LANDED`** is returned when the change id was landed from a *different* changeset
  (e.g. the same change id re-created differently); it is not reported for the merge itself.

### Landed-index semantics and side/lane commits

The merge's **side commit** (`parents[1]`, the lane head) is deliberately **not** recorded in
`landed`: it is not published as its own changeset of this ref. Later linear land attempts of the
lane history therefore see their rebases become `EMPTY_CHANGE` (their content is already in the
merged tree) and fail safely — never a double-land and never a false `ALREADY_LANDED`. Land lane
**tips** through this path; do not land lane ancestors after the merge.

## Limits (deliberately not implemented)

- Clean merges only: no conflict resolution inside this topology, no paused/continue path.
- Main/trunk refs only in practice (the façade keeps its existing `require_trunk` gate); the engine
  itself is ref-agnostic.
- A single unique merge base is required. Multiple best common ancestors still return
  `AMBIGUOUS_MERGE_BASE`; recursive/virtual merge bases are not implemented.
- No general DAG landing, no multi-parent stacks, no lane-to-lane merge-commit topology: the land
  path publishes exactly one two-parent merge. Trunk history may contain merge commits, so future
  imported/merged history must keep the tip-parent invariant.
- A merge with an empty delta is **refused** (`EMPTY_CHANGE`); this topology publishes only
  merges that change the tree. There is no "ghost merge" option in v1.

## Opting in

Deployment default (`/version` reports `merge_topology`):

```sh
FORGEVCS_MERGE_TOPOLOGY=merge-commit-v1    # default: squash-only
```

Per request (overrides the deployment default; `squash` maps to squash-only):

```json
POST /repos/<repo>/merge-land
{
  "ref": "main",
  "head": "<lane/changeset id>",
  "message": "merge lane into main",
  "topology": "merge-commit"
}
```

Success (the landed changeset is the merge itself):

```json
{
  "service": "forgevcs", "status": "ok", "repo": "<repo>",
  "merge_engine": "default",
  "merge_topology": "merge-commit-v1", "topology": "merge-commit-v1",
  "merge_changeset": { "id": "<M>" },
  "changeset": { "id": "<M>" },
  "ref": { "name": "main", "target": "<M>", "version": 7 },
  "landed": [{ "change_id": "<change id>", "source": "<M>", "result": "<M>" }]
}
```

Typed refusals (both are `409 conflict` envelopes; the façade adds `"topology"`):

- Stale tip — `details.kind = "ref_version_mismatch"` with `current_version`/`current_target`:
  re-merge onto the new tip and retry. The merge changeset is never rewritten.
- Merge conflict at creation — `code = "conflict"`, `details.kind = "merge_conflict"` with the
  usual filtered report; no paused conflict record is created. Re-mint the merge after resolving.
- Empty merge delta — `409 conflict` with a message ("merge <id> has an empty delta: its side is
  already contained in <ref>"); nothing is published and the ref does not move.
- Same change id from a different merge — `409 already_exists`, `details.kind = "already_landed"`
  (see the HTTP retry caveat above).

The squash path is unchanged (`topology: "squash"` or unset/default); its response now also
reports `merge_topology`/`topology` and keeps `changeset.id` (the linear changeset) distinct from
`merge_changeset.id` (lineage metadata).

## Implementation (vendored patch)

This is a **vendor patch**, not pristine Scion: `upstream/patches/merge-commit-v1-landing.patch`
and listed in `/version.vendor_patches` as `merge-commit-v1-landing`.

- `upstream/crates/scion-history/src/land.rs` — `History::land_merge` / `land_merge_attempt`:
  ref read → policy (`can_update`) → merge load → landed-index check (idempotent / already-landed)
  → topology gate (exactly two parents; first parent == tip, else `REF_VERSION_MISMATCH` with
  `RefMismatch { current_version, current_target }`) → `can_write` on every delta path →
  `cas_ref` with one landed row; CAS mismatch/landed conflict retry with the same full-jitter
  backoff as `land`, then `land_contention`.
- `upstream/crates/scion-history/src/types.rs` + `lib.rs` — `LandMergeRequest`, exported.
- `upstream/crates/scion-server/src/handlers/write.rs` — `pub async fn land_merge`, the narrow
  in-process entry point the ForgeVCS façade calls (same `v1::LandRequest` shape, `head` is the
  merge changeset; deliberately no proto route/RPC). Errors pass through
  `filter_conflict_payload`.
- `crates/forgevcs-service/src/config.rs` — `FORGEVCS_MERGE_TOPOLOGY`
  (`squash-only|merge-commit-v1`), validated; `/version.merge_topology`.
- `crates/forgevcs-service/src/facade/mod.rs` — `merge_land` `topology` resolution
  (`squash|merge-commit`, request overrides the deployment default; unknown → 400) and the
  merge-commit branch; squash path untouched.
- `crates/forgevcs-service/src/facade/requests.rs` — optional `topology` field.

## Tests

Upstream (`upstream/crates/scion-server/tests/write_land.rs`, `land_merge*`):

- clean tip-parented merge publishes the merge as the ref target, parents `[tip, side]`, one
  landed row (source = result = M), ref-log entry, both trees visible;
- stale first parent → `REF_VERSION_MISMATCH` with current target/version, ref unchanged, no
  landed row;
- idempotent retry after a lost CAS response (injected `meta.cas_ref.after` fault) returns the
  recorded result and adds no ref version/log row;
- non-merge changesets are `INVALID_ARGUMENT`; same change id from a different merge is
  `ALREADY_LANDED`; an empty merge delta is `EMPTY_CHANGE` and does not move the ref;
- policy denies a principal without ref `update` and a principal with ref `update` but no `write`
  on a published path;
- a conflicting merge is refused at creation (`MERGE_CONFLICT`) and publishes nothing;
- the public handler works through `CallerCtx` (the façade's entry point).

ForgeVCS (`crates/forgevcs-service`):

- `config::tests::merge_topology_defaults_to_squash_only_and_is_validated`;
- `http::tests::version_reports_the_configured_merge_topology` (+ default in
  `version_reports_pinned_upstream_commit`, and `/version.vendor_patches` includes the new patch);
- `facade::tests::merge_land_topology_request_maps_or_rejects` (request override, default,
  squash mapping, 400 on typo).
