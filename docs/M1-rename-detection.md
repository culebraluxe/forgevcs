# Rename detection — design and engine-version plan

**Date:** 2026-10-17 · **Status:** stages 1 and 2 implemented — rename awareness in both the
rebase/land path and the explicit `Merge(a,b)` path, with exact-content matching plus line-based
similarity, behind `FORGEVCS_MERGE_ENGINE=exact-rename-v1`. The default engine is unchanged.

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

## Implemented — stage 1 (rebase/land)

`upstream/crates/scion-history/src/rename.rs` detects exact renames in one side's `FileDiff`
(delete `P` + add `Q` with `P`'s base id; ambiguous candidates are left alone). `rebase.rs`
consults it when `FORGEVCS_MERGE_ENGINE=exact-rename-v1`:

- **theirs renamed** (the changeset deletes `P` and adds `Q`): the edit at `P` is resolved at
  `Q` with `base = P`, and `P` is deleted relative to the onto tree;
- **ours renamed** (the onto tip deleted `P` and added `Q`): the edit at `P` is re-anchored to
  `Q` with the onto side's content, using a lazy `FileDiff(base, onto)` only when a conflict
  candidate exists;
- **rename/rename to different destinations** returns an explicit `add_add` conflict instead of
  silently keeping both paths.

The default engine (`line-diff3-v1`, variable unset) keeps the vendored behaviour, so the
upstream gate is untouched. The pilot enables the engine for its local service and proves the
use case end to end: A lands `old.txt -> new.txt`, B had staged an edit to `old.txt` from the
pre-rename base, B lands and the edit follows the rename to `new.txt`.

## Implemented — stage 2 (merge path and similarity)

- **Similarity.** When no exact id match exists, deletions and additions of one side are compared
  by line-set Jaccard similarity; pairs at or above `0.5` are renames. Binary contents, contents
  over `merge_max_text_bytes`, redacted objects and ambiguous matches fall back to delete + add.
  Assignment is deterministic: exact matches first (unique target required), then descending
  similarity with ascending paths as the tie-break.
- **Merge path.** `Merge(a, b)` uses the same re-anchoring as rebase: a rename on either side
  resolves the other side's edit at the destination, with `base` = the pre-rename path, and the
  merge delta records the delete plus the add relative to `a`.
- Proof: the collaboration pilot lands an edit across a rename on the **merge** path (46/46), and
  the upstream unit tests cover exact, similar, unrelated, binary and ambiguous detection (18).

### Remaining limits

- Directory renames are handled per file (each unchanged file is a rename); no directory identity.
- Both sides renaming the same path to different destinations is an explicit `add_add` conflict.
- Similarity is a heuristic (line-set Jaccard, fixed threshold); a tuned engine version would
  record the threshold in history.
- Redacted or binary contents never participate in rename detection.

## Work plan

1. ~~Detection module with unit tests.~~ Done.
2. ~~Rebase integration behind the opt-in engine, with the pilot fixture.~~ Done.
3. ~~Merge integration (`Merge(a, b)`) for explicit merges.~~ Done.
4. ~~Stage 2 similarity detection.~~ Done.
5. Config/versioning: the env switch is in place; recording the merge-engine version in history
   and tuning the similarity threshold are later engine-version work.
