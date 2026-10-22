# Merge analysis — growing the merge path for agent-scale use

**Date:** 2026-10-17 · **Tree:** `4b8ca94` + the merge-land, conflict-payload and pilot changes
in this commit

> **Status note (2026-10-22):** the design proposals below are not all open work. The conflict
> `resolve`/`continue` workflow (R2) and `merge-land` shipped after this analysis; the real-work
> merge pilot (R5) is done. Still open: the merge-engine determinism gap (env-only engine not
> recorded in history), and the two-parent/lane-to-lane topology decision — both tracked in
> [`docs/PRIMARY-VCS-DOCKET.md`](PRIMARY-VCS-DOCKET.md) (P1-B/P1-C). Engine config/evidence and
> the exact upstream blocker: [`docs/MERGE-ENGINE.md`](MERGE-ENGINE.md). Topology recommendation:
> [`docs/TOPOLOGY-DECISION.md`](TOPOLOGY-DECISION.md). Conflict-write/read policy now fails closed
> for writable-but-unreadable paths (task-87, local vendored patch: `TECH_DEBT.md` TD-12).

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

## 2. What the pilot observed (41/41 green)

| Observation | Result |
| --- | --- |
| `merge-land` on a stale-base changeset | published a linear changeset distinct from the merge changeset; main carried both files |
| explicit `Merge(a, b)`, both sides add the same path | `409`, `details.kind = merge_conflict`, conflict path + kind `add_add` |
| `Land` whose automatic rebase conflicts | `409` with the same structured report (this is the ordinary push path) |
| landing a two-parent merge head | `400 invalid_argument` (`merge_in_stack`) — recorded, not a failure |
| resolution | explicit content + land: the agent chooses the bytes and lands a normal changeset; the server never writes markers |
| conflict-context | the three readable sides (`base`/`ours`/`theirs`) come back in one call for both merge and rebase orientation |

So the two conflict *sites* are (1) explicit merge and (2) every ordinary push that loses the ref
race and has to rebase. Site 2 is the one ten agents will hit constantly, and it is now observable.

## 2a. Sequential landing is the model — the advanced pieces are conditional

The intended agent loop is the simple one: **A lands, then B lands, then C lands.** Land already
implements exactly that. It serializes the ref with a version CAS; a loser rebases its linear
stack onto the new tip and retries (`land.rs:110`), which is the cherry-pick behaviour this
workflow needs. Three agents that arrive at once are handled by retry, not by a lock, and the
pilot measured it (CAS attempts 2 and 3, all `Committed`). No distributed lock, no merge commit
and no rebase UI is required for the common case.

Consequences for the roadmap — the advanced items are not on the critical path:

- **Two-parent landing** matters only if the team wants merge *commits* (lane topology retained)
  or lane-to-lane merges. A trunk-linear landing flow never needs it; `merge-land` covers the
  "integrate this head" case as a squash.
- **Recursive merge base** matters only once history contains merges: lane-to-lane merges, or
  imported Git repositories (M1 interop). A workflow that only lands onto the trunk has one best
  common ancestor by construction and can never criss-cross.
- **Rename detection** is the one engine gap that does bite the simple model: rebasing an edit
  onto a lane that renamed the same file is a false `modify_delete` conflict even though the
  workflow is purely sequential. It is the highest-value engine fix for the simple model.

## 2b. Resolution contract (shipped)

`POST /repos/{repo}/conflict-context` accepts either the merge pair (`{"a","b"}`) or the rebase
pair (`{"changeset","onto"}`) and returns the complete diff3 bundle:

| Field | Meaning |
| --- | --- |
| `orientation` | `merge` or `rebase` |
| `base`, `ours`, `theirs` | changeset ids: unique merge base / `a` / `b`, or the changeset's parent / `onto` / the changeset |
| `conflicts[]` | per path: `kind`, `hunks`, the three `entries`, and `versions` — base/ours/theirs bytes, with `{"present": false}` for an absent side |
| `clean_entries[]` | paths that merged cleanly, already resolved |

Recipe an agent follows: push/land → `409` with `details.report` → `conflict-context` with the
same arguments → choose bytes for each conflicting path → push the resolved changeset
(`mode:"land"`) → done. The server never writes markers; `versions` are capped at 1 MiB, and a
larger side is described by `size` for a `/file` read.

## 2c. Paused conflicts — resolve/continue (R2, shipped)

A refused land/merge/rebase is now **persisted** instead of being only a one-shot `409`. The
façade stores a paused conflict (`forgevcs_paused_conflicts`, in-memory for a storage-less
process) with the operation, target ref + version, the four orientation changesets
(`head`/`base`/`ours`/`theirs`), the conflict report and the readable three-way sides captured
under the caller's read policy. The initial conflict response keeps its upstream `details` and
adds a top-level `conflict` record (`id`, `operation`, `status`, `ref`, `resolve_parent`, `sides`).

```text
land / push(mode=land) / merge-land / merge / rebase
   └─ 409 + conflict{id, status: open}
        │
        ├─ GET  /repos/{repo}/conflicts?scope=paused&status=…   list paused conflicts
        ├─ GET  /repos/{repo}/conflicts/{id}                    show one (persisted sides)
        ├─ POST /repos/{repo}/conflicts/{id}/resolve            {changeset} — must be a single-parent
        │                                                        child of `resolve_parent`
        └─ POST /repos/{repo}/conflicts/{id}/continue           land the resolved changeset
```

- `resolve` only accepts an `open` conflict and validates the resolved changeset's parent; a
  second resolve, a wrong parent or a missing changeset is refused. For operations that land
  (`land`, `push` mode=land, `merge-land`) `resolve_parent` is the **ref tip observed at pause**:
  the resolution is the resolved tree on top of that tip, so `continue` lands one linear
  changeset instead of replaying the conflicting head. `merge` resolves on top of `a` and
  `rebase` on top of `onto` (its rebase target, usually the ref tip), for the same reason.
- `continue` only accepts a `resolved` conflict, checks the ref is still at the version observed
  at pause (stale → `409 conflict` with `stale:true` and the current target/version), then lands
  through the ordinary `Land` handler so write-path and ref-CAS checks still apply. If the
  resolution itself conflicts during that land (e.g. the caller moved the ref outside the
  version check's window), the `409` keeps the paused record attached instead of dropping it.
- Operations without a target ref (`merge`, `rebase`) can be resolved and are continued with an
  explicit `ref` (the CLI's `conflicts continue --ref`).
- Only the principal that hit the conflict, or a repository administrator, may resolve/continue
  it: the stored sides were read under the creator's read policy.
- A deleted repository removes its paused-conflict rows in the same metadata transaction as the
  repository/workspace/queue rows, so deletion leaves no residue.

CLI: `forgevcs conflicts list|show|resolve|continue`; failured responses keep the façade
envelope (`conflict`, `details`, `stale`, `current_*`) in the CLI error text instead of a
flattened one-line message. Tests: paused-store state machine
(open→resolved→continued, compare-and-set, unknown ids, stale/parent decisions) plus the CLI
route tests; the live end-to-end (conflict → resolve → continue → linear ref) runs in the local
Postgres+moto harness (QA evidence, not in the hermetic gate).

## 3. What is missing, in dependency order

| # | Gap | Today | Impact at 10 agents × 8 h | Where it lives |
| --- | --- | --- | --- | --- |
| 1 | No two-parent landing | `merge-land` squashes the merge result into one linear commit | **conditional**: only if merge-commit topology is wanted; trunk-linear landing never needs it | upstream `land.rs` / façade policy |
| 2 | Resolution workflow | **shipped (R2)**: `conflict-context` (§2b) plus persisted paused conflicts with `resolve`/`continue` (§2c) | none for the simple model; an agent still chooses the bytes | façade + client contract |
| 3 | Criss-cross histories | `AMBIGUOUS_MERGE_BASE` (SPEC line 1479, future row line 1757) | **conditional**: only if merges exist (lane-to-lane or imported Git); trunk-linear history cannot criss-cross | merge engine version bump |
| 4 | Rename engine: shipped opt-in, default off | `exact-rename-v1` (exact + similarity) is implemented for land/rebase and `Merge` behind `FORGEVCS_MERGE_ENGINE` (`upstream/crates/scion-history/src/rename.rs:34-40`, `merge.rs:122`, `rebase.rs:80`; TD-1 resolved). The default process runs the old delete+add behaviour, so a deployment that forgets the env silently gets false `modify_delete` conflicts; the engine version is not recorded in history | **managed config + recording**: validated/reported by `/version` and land evidence (2026-10-22), history recording blocked upstream (`docs/MERGE-ENGINE.md`); residual directory identity / both-sides-rename / threshold tuning is TD-1 | merge engine config + upstream engine-version mechanism |
| 5 | Merge-aware policy: fail-closed patch shipped | merge/land require `write` on every delta path while conflict payloads are filtered by `read` (`upstream/crates/scion-server/src/handlers/write.rs:69`). **Local ForgeVCS patch (task-87, 2026-10-22):** if any conflicting path is writable but unreadable, the operation now fails closed with a generic `PERMISSION_DENIED` naming neither path nor content instead of returning a partial report. Clean entries and read-only hidden paths keep the omission behaviour (a reader that cannot write them could not resolve them). A write-but-not-read principal can no longer receive a conflict it cannot see; it must be given read (or a different principal must resolve) | **closed for the write-not-read defect**; residual: read-only omissions still hide paths from readers who cannot resolve them anyway (no escalation marker — option C was rejected), and the patch is a local vendored divergence (TD-12) | policy model + local upstream patch |
| 6 | Merge observability: registry wired, hosted scrape/alerting missing | **Lane U (2026-10-22)**: one process-wide `scion_obs::Metrics` is created in `main`, injected into every engine (`engine.rs` `Engine::connect(…, Arc<Metrics>)`) and served by `/metrics` (`http.rs`), so `scion_land_attempts{result}`, `scion_land_duration_seconds` and `scion_ref_cas_failures_total{ref}` reflect real land/CAS traffic instead of an all-zero registry. Conflict-workflow counters added in `crates/forgevcs-service/src/metrics.rs`: `forgevcs_conflict_pauses_total{operation}`, `forgevcs_conflict_resolves_total`, `forgevcs_conflict_continues_total{result}` (names/labels in `docs/OBSERVABILITY.md`). | questions like "how often do agents conflict, on what, how long does land take" are answerable from one scrape; **remaining**: no hosted scrape, dashboard or alert rule consumes `/metrics` (Vercel does not scrape), and there are still no distributed traces | service registry (done) + hosted Prometheus scrape/dashboards/alerts |
| 7 | Large/binary content | `merge_max_text_bytes` turns large text into `binary` conflict; binary = whole-file ours/theirs | agent must resolve whole files by hand | engine limits / UX |
| 8 | Scale evidence | single-process pilot, 6 repos, seconds | no error-rate/latency data for ten agents | soak + hosted |

Rows 1 and 3 are conditional on the topology decision in §2a; row 4 is the one the simple
sequential model needs. Row 2 is resolved by this commit. Row 6's **service** half is resolved by
Lane U (one shared registry, real land/CAS series, conflict counters); its **hosted** half (scrape,
dashboards, alert rules — see `docs/OBSERVABILITY.md` §4) is still open.

## 4. Adapt at the edges, or grow the engine?

The project's hard rule is that `upstream/` is vendored Scion: adapt it at the edges, do not
rewrite its behaviour, and keep its gate green. That splits the roadmap cleanly:

- **Edge (ForgeVCS, this repo):** merge-land, structured conflict payload and conflict-context
  (shipped), retry/idempotency, metrics, soak harness. No upstream change.
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

1. **Resolution contract — shipped** (gap 2): `conflict-context` plus the recipe in §2b.
2. **Rename detection — shipped opt-in, config now managed** (gap 4): `exact-rename-v1` removes
   most false `modify_delete` conflicts on land/rebase and `Merge`; the deployment must set
   `FORGEVCS_MERGE_ENGINE=exact-rename-v1` (validated, `/version`-reported, land evidence
   recorded — `docs/MERGE-ENGINE.md`). Remaining: record the engine version in generated history
   (upstream blocker) and the TD-1 residual limits.
3. **Merge/conflict metrics and the N-agent soak** (gaps 6, 8): first connect the service metrics
   registry to the engines and add conflict counters (packet P0-6), then run real agents and
   record conflict rate, land retries, and latency.
4. **Two-parent land** (gap 1) only if merge-commit topology is wanted; then `merge-land` can
   choose merge-commit vs squash explicitly.
5. **Recursive merge base** (gap 3) when lane-to-lane merges or imported Git history exist; then
   Git mirror/import interop.
6. **Repository deletion, merge-aware policy, rate limits** (gaps 5 and the M1 P0 list).

The simple model needs items 1–3. Items 4–5 are real but conditional on the topology decision;
they are not prerequisites for ten agents landing sequentially, which is the workflow the
migration starts with.
