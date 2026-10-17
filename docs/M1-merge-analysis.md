# Merge analysis — growing the merge path for agent-scale use

**Date:** 2026-10-17 · **Tree:** `4b8ca94` + the merge-land, conflict-payload and pilot changes
in this commit

Merge is the top of the mountain for the migration: git's merge/index/worktree behaviour is what
broke at three bots, and the replacement has to be *better*, not merely different. This document
records what Scion actually implements, what the pilot observed, and the ordered work to make it
robust. It is analysis plus the two changes this commit shipped; the big items are design
proposals, not claims of completion.

## 1. The model we inherited from Scion

### The merge itself (SPEC 8.3, `scion-history/src/merge.rs`)

`Merge(a, b)` computes the unique best common ancestor of the two changesets and applies a
per-path three-way rule with `b = base`, `o = ours (a)`, `t = theirs (b)`:

| Condition | Result |
| --- | --- |
| `o == b` / `t == b` / `o == t` | take the changed side (or the shared side) |
| both sides add a different entry | conflict `add_add` |
| one side deletes, the other changes | conflict `modify_delete` |
| differing symlinks / binary or oversized content / redacted side | conflict `symlink`, `binary`, `redacted` |
| file vs directory structure | conflict `file_directory` |
| otherwise | content merge with `line-diff3-v1` (Myers, deterministic tie-break); overlapping or adjacent hunks conflict unless identical |

Two properties matter more than the table:

1. **Conflict markers are never written into server-side objects.** Conflicts are returned as
   data: a `ConflictReport { clean_entries, conflicts }`, where each conflict is
   `{path, kind, base?, ours?, theirs?, hunks?}` (`scion-core/src/conflict.rs:21`–`113`).
2. **The result is deterministic**: identical `(a, b, merge engine version)` produce identical
   identifiers. That is what makes retries and idempotent operators possible.

A two-parent merge changeset is created; its delta is computed against `a` and its parents are
`[a, b]`. More than one best common ancestor (a criss-cross history) returns
`AMBIGUOUS_MERGE_BASE` instead of guessing.

### Rebase (SPEC 8.2, `rebase.rs`)

`Rebase(C, X)` replays one single-parent changeset onto `X` using the same per-path rules.
Exactly the paths of `C.delta` are examined; conflicts are collected and returned as the same
`ConflictReport`. A multi-parent changeset cannot be rebased.

### Land (SPEC 8.4, `land.rs`)

`Land(ref, head)` takes a **linear stack**: the first-parent chain from `head` back to the first
changeset that is an ancestor of the ref tip. Every member must have exactly one parent; a
multi-parent member fails `INVALID_ARGUMENT/merge_in_stack` (`land.rs:94`, `land.rs:451`). Land
then rebases remaining members onto the tip, checks `update` on the ref and `write` on every
delta path **on every attempt**, and commits the ref with a version CAS. Losing the CAS race
backs off and retries (`land.rs:110`); the `landed` ledger makes retries idempotent.

This is why the pilot saw two different outcomes that look like they should agree:

- three agents pushing disjoint paths to one ref → **all land**, with land CAS attempts 2 and 3;
- two agents pushing the same path → one lands, the other gets `409 conflict`.

And it is why a merge changeset cannot be landed directly: it is a legal changeset but not a
legal *stack member*.

### The conflict payload stopped at gRPC

`scion.v1.ErrorDetail` carries the payload in the gRPC status details (`scion-proto/src/status.rs`);
`handlers/write.rs:69` filters it by read policy so an agent never learns paths it cannot read.
The ForgeVCS REST façade was mapping only the status code and message, so every merge conflict
arrived as `{"code":"conflict"}` with no paths, kinds or hunks. **Fixed in this commit**:
`status_response` now decodes the `ErrorDetail` and returns `details` — `merge_conflict` with
the full report, `ref_version_mismatch`, `already_landed`, limits, subcodes and candidates.

## 2. What the pilot observed (38/38 green)

| Observation | Result |
| --- | --- |
| `merge-land` on a stale-base changeset | published a linear changeset distinct from the merge changeset; main carried both files |
| explicit `Merge(a, b)`, both sides add the same path | `409`, `details.kind = merge_conflict`, conflict path + kind `add_add` |
| `Land` whose automatic rebase conflicts | `409` with the same structured report (this is the ordinary push path) |
| landing a two-parent merge head | `400 invalid_argument` (`merge_in_stack`) — recorded, not a failure |
| resolution | explicit content + land: the agent chooses the bytes and lands a normal changeset; the server never writes markers |

So the two conflict *sites* are (1) explicit merge and (2) every ordinary push that loses the ref
race and has to rebase. Site 2 is the one ten agents will hit constantly, and it is now observable.

## 3. What is missing, in dependency order

| # | Gap | Today | Impact at 10 agents × 8 h | Where it lives |
| --- | --- | --- | --- | --- |
| 1 | No two-parent landing | `merge-land` squashes the merge result into one linear commit; merge lineage lives only in the returned `merge_changeset` | history cannot show *why* a change landed (merge vs direct); audit and bisect get weaker | upstream `land.rs` / façade policy |
| 2 | No resolution workflow | conflict report is data; base/ours/theirs must be fetched by hand (`merge-base` + `/file`); resolution = write bytes + land | every agent re-invents the same recipe; one wrong step silently resolves the wrong side | façade + client contract |
| 3 | Criss-cross histories | `AMBIGUOUS_MERGE_BASE` (SPEC line 1479, future row line 1757) | long-lived agent lanes that merge each other will eventually criss-cross; merges start failing | merge engine version bump |
| 4 | No rename/copy detection | renames are delete + add; merge treats them as such (SPEC line 1756, future row) | moving a file in one lane while another edits it becomes a false `modify_delete` conflict | derived type + merge engine version bump |
| 5 | Merge-aware policy | merge needs `write` on every delta path; conflict payload is filtered by `read` (write.rs:69) | a path an agent may write but not read produces a filtered/hidden conflict — the agent cannot see what it must decide | policy model |
| 6 | No merge observability | no conflict/land metrics, no traces; only logs | cannot answer "how often do agents conflict, on what, and how long does it take" during the migration | server + Vercel |
| 7 | Large/binary content | `merge_max_text_bytes` turns large text into `binary` conflict; binary = whole-file ours/theirs | agent must resolve whole files by hand | engine limits / UX |
| 8 | Scale evidence | single-process pilot, 6 repos, seconds | no error-rate/latency data for ten agents | soak + hosted |

## 4. Adapt at the edges, or grow the engine?

The project's hard rule is that `upstream/` is vendored Scion: adapt it at the edges, do not
rewrite its behaviour, and keep its gate green. That splits the roadmap cleanly:

- **Edge (ForgeVCS, this repo):** merge-land, conflict payload, conflict-context endpoint,
  retry/idempotency, metrics, resolution contract, soak harness. No upstream change.
- **Engine version (Scion semantics):** two-parent landing, recursive merge base, rename
  detection. These change merge semantics and need the SPEC's engine-version mechanism and
  differential tests against the reference model — they belong upstream, vendored as a new
  baseline, not patched in place.

## 5. Design sketches for the three hard ones

### A. Two-parent landing (gap 1)

Land's linear-stack rule exists because land *rebases* stack members. A merge head needs no
rebase when its first parent already reaches the tip: it is already a descendant of `tip`. The
minimal engine change is to accept a multi-parent member when the stack walk can prove it needs
no rebase (all parents are ancestors of the merge head and the first-parent chain reaches
`tip`), then CAS the ref to the merge head unchanged. This does not weaken the linear path and
makes `Merge` publishable as a real merge commit. Façade option until then: keep `merge-land`
(squash) and record the merge changeset id in commit metadata so lineage is discoverable.

### B. Recursive/virtual merge base (gap 3)

When `merge_bases(a, b)` returns more than one id, merge the bases pairwise (recursively) into a
virtual base tree, then run the ordinary three-way merge against that tree. Conflicts found while
building the virtual base must be carried into the final report rather than discarded. This is
the git `recursive`/`ort` strategy; SPEC already lists it as a later engine version.

### C. Rename detection (gap 4)

Derive a rename map between base and each side by content id (exact renames first), then apply the
three-way rules per base path/side path pair; a rename in one side plus a modification in the other
is a clean merge, not `modify_delete`. A similarity threshold for edit-renames is v2; exact-content
renames alone remove most agent-move conflicts.

## 6. Recommended order

1. **Conflict context endpoint + resolution contract** (gap 2, no upstream change): return the
   conflict report already in the 409, plus the base/ours/theirs changesets so a client can read
   all three sides; document the resolution recipe (`base = merge-base(a,b)` → read sides →
   write resolved content → land). This unblocks real agents now.
2. **Merge-land hardening**: re-merge if the tip moved between resolve and land (retry loop),
   idempotency via a caller-supplied change id, and a metric per landing.
3. **Two-parent land** (gap 1) as the first engine-version bump; then `merge-land` can choose
   merge-commit vs squash explicitly.
4. **Recursive merge base** (gap 3) — the largest robustness win for ten agents.
5. **Rename detection** (gap 4), then merge/conflict metrics and the soak run (gaps 6, 8), then
   per-repository ACL (gap 5) and repository deletion.

The first item is the one that makes the system usable by real agents; the fourth is the one that
makes it *robust* at the scale that broke git.
