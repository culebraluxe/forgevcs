# M1 — three-agent pilot and the collaboration surface

**Date:** 2026-10-17 · **Tree:** `fc18858` + the collaboration endpoints and per-agent refs
in this commit

The first multi-principal acceptance for the migration question: *can a few agents work at
once without the git collision that stalled the lane model?* It runs one local engine, three
service principals, an independent repository each, a shared ref they all push to, and one
team repository where each agent owns a lane ref.

## Run it

```sh
scripts/agent-pilot.sh              # .env.local, port 3998
PORT=4100 scripts/agent-pilot.sh
```

The script builds the service, boots it with `FORGEVCS_SERVICE_PRINCIPALS=agent-a/b/c`, and
drives `scripts/agent-pilot.py`. It loads `scripts/pilot/acl.toml` — a local policy that names
the three worker principals, the DevOps seat and their lane refs — so the committed `acl.toml` and the hosted
deployment's policy are unchanged. The principals are throwaway deterministic secrets; no
deployment configuration is touched.

**It creates six repositories** in the Neon branch behind `.env.local`, and there is still no
repository deletion endpoint, so see "Cleanup" below.

The merge path itself is analysed in `docs/M1-merge-analysis.md`: the inherited Scion model,
the conflict sites the pilot exercises, and the ordered work to grow it.

## Result — 2026-10-17, PASS 41/41

```text
identity      each worker and the DevOps seat mint a token; /auth/whoami names each
lifecycle     the DevOps seat creates the repositories; worker creation is proven in role-pilot
isolation     INFO agent-a read agent-b's repo refs (HTTP 200) — recorded gap, not a failure
independent   three agents advanced three separate repositories by 3 changesets
shared-ref    three disjoint pushes to one ref all landed; the tree carries every file
contention    two same-path pushes: exactly one landed, the other got a clear 409, no lost ref
recovery      the conflicted agent landed on the winner's tip
workspaces    three agents created, wrote and submitted a workspace concurrently
refs          admin.refs provisioned one lane ref per agent; each agent landed its own ref
write scope   agent-b's push to agent/a was refused: 403
land          Land rebased agent-b's lane, then agent-a's, onto main; both files coexist
merge         the merge handler produced a two-parent changeset from the two lane heads
rebase        a fresh changeset was replayed onto main and landed
merge-land    a stale-base changeset merged into main and landed as one linear changeset
conflict(m)   explicit Merge over the same added path: 409, details carry path and add_add
conflict(l)   Land's rebase-on-contention: 409, details carry path and add_add
context(merge) base/ours/theirs readable in one call for the merge pair
context(land)  base=parent, ours=tip, theirs=head readable in one call for the rebase pair
resolution    explicit content + land; the server never writes conflict markers
OK  41 checks passed across 3 agents, the DevOps seat and 6 repositories
```

The engine log carries the mechanism, not just the verdict. In the shared-repository round the
three disjoint pushes landed as:

```text
land finished ref_name=main attempt=1 version=2 landed=1 outcome=Committed
land finished ref_name=main attempt=2 version=3 landed=1 outcome=Committed
land finished ref_name=main attempt=3 version=4 landed=1 outcome=Committed
```

Attempts 2 and 3 are the lock-free Land CAS losing the race and retrying; each retry rebases
the contended changeset onto the new tip and commits. **No agent's work was lost and no agent
was rejected for losing the race.** The same-path case is the honest counterexample: one push
commits, the other returns `409 conflict`, the ref keeps the winner's bytes, and the loser
recovers by landing on the new tip.

## Collaboration surface added

| Route | Upstream handler | What it gives an agent |
| --- | --- | --- |
| `POST /repos/{repo}/refs` | `RefService.CreateRef` | Provision a lane ref (requires `admin.refs`; `GET` on the same route still lists refs) |
| `POST /repos/{repo}/rebase` | `WriteService.Rebase` | Replay a changeset onto another; returns the new changeset |
| `POST /repos/{repo}/merge` | `WriteService.Merge` | Produce a merge changeset from two changesets |
| `POST /repos/{repo}/land` | `WriteService.Land` | Publish an existing changeset on a ref (linear stack, rebased on contention) |
| `POST /repos/{repo}/merge-land` | Merge + CreateChangeset + Land | Merge a head into the ref tip and publish the result as one linear changeset (the v1 Land stack is linear, so a two-parent merge head cannot be landed directly) |
| `POST /repos/{repo}/conflict-context` | Merge/Rebase + `read_path` | The three readable sides of a refused merge or rebase as one diff3 bundle (`versions.base/ours/theirs`, entries, hunks) |

All six go through the vendored handlers with the caller's `CallerCtx`, so policy, conflict
payloads and land semantics stay upstream. The façade only adds the JSON shapes. Merge conflicts
are returned with their `ErrorDetail` payload as `details` (paths, kinds, entries, hunks) instead
of a bare `{"code":"conflict"}`.

## Per-agent write scope is a REF scope

The lane replacement is not a worktree: each agent may advance only its own ref
(`agent/a`, `agent/b`, `agent/c`), provisioned once by an `admin.refs` seat, with `main` as
the shared integration target. Policy in `scripts/pilot/acl.toml`:

```toml
[groups]
agent-a = ["service:agent-a"]   # and -b, -c; provisioner = agent-a
[refs]
"main"    = { update = ["group:agents"] }
"agent/a" = { update = ["group:agent-a"] }   # and -b, -c
[admin]
refs = ["group:provisioner"]
```

Agent-b passes the shared push pre-check and can write any path, but the *ref update* is
refused: that 403 is the write scope. Paths stay shared because a real codebase shares files
across agents; the isolation is in history, not directories.

## What this proves

- Per-agent identity, independent repositories, and concurrent workspaces.
- Same-ref, disjoint-path pushes all land; contention is serialized with a clear 409 and no
  lost ref.
- Per-agent refs provisioned by an admin seat; cross-ref writes refused; lanes integrated by
  `land` with automatic rebase.
- Explicit `merge`, `rebase` and `merge-land` endpoints are reachable and return real changesets;
  `merge-land` integrates a stale-base changeset with a merge result.
- Both conflict sites are observable: explicit `Merge` and Land's automatic rebase return `409`
  with the structured `details.report` (path, kind, base/ours/theirs entries, hunks), and
  `conflict-context` returns the three readable versions in one call. A chosen resolution lands
  as an ordinary changeset.
- Retry recovery: the loser can land on the new tip, with no half-applied state.

## What this does not prove

- **Per-repository isolation.** `acl.toml` is path/ref-scoped, not repo-scoped: policy does not
  see the repository name. agent-a reading agent-b's repository remains a recorded INFO and is
  the M1 P0 gap in `docs/M1-readiness-and-multi-agent.md`.
- **Path-prefix write scopes on `push`.** Scion checks write on every changeset delta path in
  `create_changeset`, but the façade's `push` first requires write on `/` (the
  `authorize(... Action::Write, Some("/") ...)` before uploads). Requiring write on `/` means
  every agent needs write everywhere, so path prefixes cannot restrict pushes yet. Relaxing
  that coarse gate is the enabling change for path scopes; ref scopes do not need it.
- **Landing a merge head directly.** `POST /land` on the two-parent merge changeset returns
  `400 invalid_argument` (`merge_in_stack`): Land intentionally takes a linear stack.
  `merge-land` works around it by publishing the merge result as a linear changeset; real
  two-parent landing is an engine-version item in `docs/M1-merge-analysis.md`.
- **A resolution workflow.** There is no paused-conflict state or `resolve`/`continue` endpoint.
  Today a conflict is resolved by reading the sides (`merge-base` + `/file`), writing the chosen
  bytes as a changeset, and landing; the analysis records the missing contract.
- **Hosted/multi-instance behavior, deletion, rate limits, soak.** One local process on one
  Neon branch. The pilot also hit one transient `503` from the object store during repo init
  (retried successfully), which is exactly the retry/soak territory the migration needs.

## Findings worth carrying into M1

1. Rebase-on-land is real and handles the common multi-agent case. The gap is not "concurrent
   pushes fail"; it is conflict *resolution* and publishing merge results.
2. The write scope that maps to the old lane accounting is a ref scope, not a path scope; it
   needs no server change, only an `admin.refs` provisioning step.
3. The façade used to drop the `ErrorDetail` payload; it now returns structured conflicts, which
   is what makes agent-driven resolution possible at all.
4. The façade's coarse `write on /` gate on `push` still blocks per-path scopes; the engine
   already performs the finer check.
5. No deletion endpoint means every pilot run is permanent. This workstation's runs created
   about 30 test repositories; the deletion workstream is a prerequisite for routine piloting.
6. Two transient failures surfaced (an object-store `503` during init and one retried run);
   a sustained run needs retry policy and load data before ten agents share the service.

## Cleanup

Created by this workstation's pilot runs on 2026-10-17 (no deletion endpoint; remove with SQL
against the Neon branch). Each stamp below produced `-a`, `-b`, `-c`, `-shared` and, from the
fifth run on, `-team` repositories:

- `agent-pilot-2610171727059d7e` — first harness run, stopped at the recovery check
- `agent-pilot-261017172902fd75` — passing 20-check run
- `agent-pilot-2610171742132a96` — stopped at the nested-tree check
- `agent-pilot-2610171744587d76` — stopped at a client-side name shadowing
- `agent-pilot-2610171747424fcf` — stopped at a transient `503` during init
- `agent-pilot-261017174823f50f` — passing 31-check run
- `agent-pilot-26101719085177b6` — passing 38-check run (`-a`, `-b`, `-c`, `-shared`, `-team`, `-merge`)
- `agent-pilot-26101719263382f3` — passing 40-check run (adds `conflict-context`)
- `agent-pilot-261017194653bceb` — passing 41-check run (repositories created by the DevOps seat)

Repositories are rows in the Scion metadata tables plus object keys in the bucket; deleting one
means every Scion table row for its `repo_id` plus the object keys (the same reviewed
destructive path `docs/W4-engine-wiring.md` §8.6 records as absent).

## Next

1. Rename detection — designed in `docs/M1-rename-detection.md`; the one engine fix the
   simple sequential model needs (a rename plus an edit in another lane currently lands as a
   false `modify_delete` conflict).
2. Merge/conflict metrics and the N-agent soak with real DeepSeek agents.
3. Harden `merge-land` (re-merge when the tip moves; caller-supplied change id) and then decide
   whether two-parent landing (`docs/M1-merge-analysis.md` §5A) is wanted for topology.
4. Recursive merge base only when lane-to-lane merges or imported Git history exist.
5. Then repository deletion (gated on the DevOps seat, per `docs/M1-roles.md`),
   per-repository ACL and the remaining M1 P0 list.
