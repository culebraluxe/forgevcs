# Topology decision memo: squash-only vs two-parent / lane-to-lane merge commits

**Date:** 2026-10-22 · **Status:** recommendation for principal sign-off; no engine change in this
work order. Inputs: `docs/M1-merge-analysis.md` §2a/§5, `docs/PRODUCTION-PACKET.md` P1-3, Lane Q
review (2026-10-22).

## The current contract

- **Land is linear by design.** `History::land` accepts only a first-parent chain of single-parent
  changesets; any multi-parent (or parentless) member fails `INVALID_ARGUMENT/merge_in_stack`
  (`upstream/crates/scion-history/src/land.rs:450-473`). This is what makes the retry/CAS model
  work: a loser rebases its linear stack onto the new tip (`land.rs:110`).
- **Explicit merge produces a real two-parent changeset** (`merge.rs` creates parents `[a, b]`) but
  that changeset **cannot be published by the current land path**. The façade's `merge-land`
  instead re-derives a single-parent linear changeset from the merge delta and lands that
  (`facade/mod.rs:1578-1591`) — a squash with the merge changeset returned as lineage metadata.
- **A linear trunk can never criss-cross**; `Merge` of histories that contain merges can return
  `AMBIGUOUS_MERGE_BASE` (`upstream/crates/scion-history/src/merge.rs:56-64`), and `conflict-context`
  refuses more than one best common ancestor (`facade/mod.rs:1997`).
- **Lane-to-lane today** is "integrate lane Y into lane/ref X": call `merge-land` with `ref: X`,
  `head: Y`; the result is a squash on X. No merge-commit topology is retained; the returned
  `merge_changeset` id keeps lineage referencable but is not itself published.

## Options

| | A. Squash-only (current) | B. Two-parent landing | C. Recursive base + lane-to-lane merge commits |
|---|---|---|---|
| What changes | none; document the contract | `land` accepts a multi-parent member when no rebase is needed (first-parent chain already reaches tip), then CASes the ref to the merge head; `merge-land` chooses squash vs merge-commit mode | B plus recursive/virtual merge base when there is more than one best common ancestor |
| Trunk history | linear | contains merge commits | contains recursive merges |
| Retry/CAS semantics | simple: rebase stack, retry | needs care: a stale merge head must be re-merged (not just rebased); CAS retry must re-run merge or fail with a typed stale-merge error | plus conflicts during virtual-base construction carried into the report |
| Criss-cross / imported Git | impossible on a linear trunk; imported history is replayed linearly by the mirror | possible only once merge commits exist; may need C | supported |
| Agent workflow | `merge-land`/paused continue; integration is a squash | agents must choose merge vs squash; review/PR surfaces need topology UI | full lane topology; heaviest |
| Engine work | none | upstream `land`/`merge` semantics + SPEC engine version + tests | B + recursive base engine work + differential tests |
| Risk | simplest, proven by pilots; topology lost | new failure modes (stale merge, merge-in-stack relaxation, policy on two parents) | largest; only justified by real imported-merge/lane-topology need |
| Status | **shipped and used** | conditional, deferred (`M1-merge-analysis.md:173`) | conditional, deferred (`:175`) |

## Recommendation

**Adopt A (squash-only) as the production contract for the agent lanes now, and keep B/C behind an
explicit product trigger.** Reasons:

1. The actual workflow is sequential landing into a trunk (`M1-merge-analysis.md` §2a): one writer
   wins, the next rebases and retries. Agents do not need merge-commit topology to converge; the
   pilot evidence (41/41, 46/46, R5 33/33) is all linear-landing behaviour.
2. A linear trunk eliminates criss-cross and recursive-base risk by construction. The one-way git
   mirror imports history linearly, so imported merge history does not enter ForgeVCS today.
3. B changes publication semantics — stale merge heads, re-merge-on-CAS, merge-aware policy for two
   parents — and needs an upstream engine version plus differential-equivalence tests. It is not a
   façade patch, and doing it speculatively adds the failure modes we are trying to remove.
4. Lane-to-lane integration already has a correct squash path (`merge-land` into the target ref);
   the returned `merge_changeset` keeps lineage discoverable. If a team needs visible branch topology
   or reviewable merge commits, that is a product decision with a concrete cost, not a bug fix.

**Triggers to reopen B (and then C):**
- the principal wants merge commits / non-linear primary history for humans;
- lane-to-lane merges must preserve topology rather than squash;
- imported Git history with existing merges becomes a supported source (then criss-cross handling is
  mandatory, not conditional);
- a real-work pilot needs to resolve a conflict once and publish both sides' lineage visibly.

## Migration path if B is chosen (estimate: engine-version work, not a façade patch)

1. **Design/SPEC:** define the engine version that accepts a merge head; `land` may accept a
   multi-parent member only when its first-parent chain already reaches the tip and no rebase is
   needed; otherwise return a typed "needs re-merge" error.
2. **Engine:** change `land.rs` stack walk/validation and stale-head handling; make merge-land
   re-merge (not rebase) on CAS loss; add engine-version recording per `docs/MERGE-ENGINE.md`.
3. **Façade:** `merge-land` gets an explicit `mode: squash|merge` (default stays squash for
   compatibility); responses report which topology was published; conflict/paused workflow handles
   two-parent resolutions.
4. **Tests:** two-parent landing, stale merge head re-merge, CAS retry, merge-aware policy on both
   parents, criss-cross still refused, differential against the reference model; update topology memo
   and packet.
5. **Rollback:** keep squash as the default and the current engine value; the feature is opt-in per
   request/deployment until a second engine version is accepted.

C (recursive base) follows B only when imported merge history or lane topologies make criss-cross
real; it reuses the same engine-version machinery and adds virtual-base conflict propagation.
