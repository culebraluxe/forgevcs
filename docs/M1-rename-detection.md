# Rename detection — design and engine-version plan

**Date:** 2026-10-17 · **Status:** design only. Nothing in this document is implemented yet.

## Why this is the next engine work

Scion detects no renames. SPEC line 1756 records it plainly: *"Rename/copy detection: None;
merge treats renames as delete + add"*, with a derived type and a merge-engine version bump as
the future work. In the sequential landing model — A lands, B rebases onto A, C onto B — a
rename is exactly the case that produces a **false** conflict:

```text
main:       P exists
agent A:    lands a rename  P -> Q   (delta: delete P, add Q with P's blob)
agent B:    later edits P            (delta: modify P)
B lands:    Land rebases B's modify-P onto main, where P is gone
            -> conflict modify_delete, even though both agents did the right thing
```

Without rename detection the only resolution is manual: the agent re-creates the edit at `Q`.
With it, B's modification follows the rename and lands cleanly. This is the one merge gap that
bites the simple workflow; recursive merge base and two-parent landing are conditional on a
topology (merges/imports) that the trunk-linear model does not have.

## Why this belongs in the engine, not the façade

The conflict is produced inside `History::rebase` (used by `Land`) and `History::merge`, from
per-path three-way rules (`conflict.rs`). A façade cannot fix it: the changeset model has no way
to say "this path's base content lives at another path", so re-parenting or rewriting a delta
before the call cannot reproduce the correct base. Rename detection changes the merge rule itself;
per the house rule it is vendored-engine work with an engine-version bump, not an edge adapter.

## Stage 1 — exact-content renames

Detect a rename only when the content id is unchanged: `Delete(P)` and `Add(Q)` in the same
side's `FileDiff(base, side)` where `Q.id == P.id` (P's base entry). This is deterministic,
cheap, and covers the common agent action (move/rename a file without editing it in the same
change). Similarity renames are stage 2.

### Detection

For each side (ours = `a`, theirs = `b`), build `rename: BTreeMap<base_path, side_path>` and the
inverse `claimed: BTreeSet<side_path>` from the already-computed `FileDiff(base, side)`:

1. Collect `deleted` paths (old present, new absent) with their base entry id.
2. Collect `added` paths (old absent, new present) by entry id.
3. For each deleted path `P`, if exactly one added path `Q` has the same id, record `P -> Q`
   (ambiguous candidates are left as delete + add, i.e. current behaviour).

Both diffs are already computed by `merge`, so detection adds one pass over them, no extra tree
reads. For `rebase`, the diff is **not** currently computed (rebase reads only `C.delta` paths for
cost); rename detection there needs a lazy `file_diff(base_root, onto_root)` computed only when a
delta path conflicts with a delete. That cost is bounded by the divergence between the lane and
the trunk, and it is paid only on a conflict, not on the clean path.

### Merge rules with a rename map

Canonicalise every examined path through the maps before applying SPEC 8.3. The new cases:

| Situation | Result |
| --- | --- |
| ours renames `P -> Q`, theirs unchanged at `P`, ours edited `Q` | take `Q` (ours) |
| ours renames `P -> Q`, theirs modifies `P` | result `Q` = merge(base=P, ours=Q, theirs=P-content); clean when only one side changed |
| both sides rename `P` to the same `Q` | one `Q`; content merged if both edited it |
| both sides rename `P` to different `Q1`, `Q2` | conflict `add_add` at both destinations (explicit, not silent) |
| ours renames `P -> Q`, theirs deletes `P` | result: ours wins with a conflict `modify_delete` (or a policy choice: rename wins, reported as `rename_delete`) |
| rename destination collides with an existing path on the other side | `file_directory`/`add_add` conflict; never overwrite silently |

Every conflict keeps the existing shape (`path`, `kind`, `base/ours/theirs`, hunks), so the REST
conflict payload and `conflict-context` keep working unchanged.

### Rebase rules with a rename map

Rebase replays `C.delta`; the rename lives on the onto side (across arbitrary history) or in `C`
itself:

- **rename on the onto side, ours-side delete + theirs modify**: on a `modify_delete` candidate at
  `P`, lazily diff `base -> onto`, find whether `P` was renamed to `Q` by id, then resolve at `Q`
  with `base = P` (from C's parent), `ours = Q` (from onto), `theirs = C's new P content`.
- **rename in `C` (theirs side)**: `C.delta` itself has delete `P` + add `Q` with the same id;
  pair them, then resolve at `Q` with `base = P`, `ours = onto's P content`, `theirs = Q`.
- The resulting delta path is rewritten to the canonical path, so Land publishes `delete P` +
  `add Q` with the merged content.

Determinism holds: the maps are functions of tree content ids and path order; no clocks or
similarity scores enter stage 1.

## Stage 2 — similarity renames (later)

A derived type (`rename/copy detection`, SPEC line 1735) scoring content similarity (e.g. 50%
threshold, git-compatible defaults), consumed only when an exact match is absent. It must be
versioned (`DerivedType.version`) and may not change stage-1 results. This is where edit-renames
(rename + edit in the same change) become detectable.

## Engine versioning and the vendored gate

- The merge engine is currently the implicit `line-diff3-v1` (SPEC 8.3). Add `merge_engine =
  "rename-diff3-v1"` as a `History`/`Config` option; **default stays the current behaviour**, so
  the vendored gate (1343 tests) stays green and existing fixtures do not move.
- New behaviour is exercised by new engine-version tests, and ForgeVCS enables it through
  `FORGEVCS_MERGE_ENGINE` for the pilot.
- Because change ids are content-derived and engine-version-sensitive, any engine that produces
  history must record the version it used; the option is part of the composition, not a
  per-request flag.
- Differential harness: extend the upstream reference-model differential tests with rename cases
  (rename/rename, rename/modify, rename/delete, directory rename, case-only rename, rename into a
  directory that exists on the other side).

## Work plan

1. Detection module (`scion-history/src/rename.rs`) with unit tests for exact renames and
   ambiguity fallback — no behaviour change yet.
2. Merge integration, gated by the new engine option, with the case table above as tests.
3. Rebase integration (the Land path) with the lazy onto-side diff; this is the case the pilot
   will exercise end to end.
4. Config/versioning plumbing (`FORGEVCS_MERGE_ENGINE`, recorded in docs), differential tests,
   and a pilot check: A renames, B edits, B lands cleanly.
5. Stage 2 (similarity) as a separate engine-version increment.

Steps 1–4 are the rename work the class of agent workflows needs; step 5 is an enhancement.
