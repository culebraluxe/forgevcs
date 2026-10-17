# M1 — three-agent pilot

**Date:** 2026-10-17 · **Tree:** `2681ead` + the pilot harness in this commit

The first multi-principal acceptance for the migration question: *can a few agents work at
once without the git collision that stalled the lane model?* It runs one local engine, three
service principals, and a shared repository they all push to.

## Run it

```sh
scripts/agent-pilot.sh              # .env.local, port 3998
PORT=4100 scripts/agent-pilot.sh
```

The script builds the service, boots it with `FORGEVCS_SERVICE_PRINCIPALS=agent-a/b/c`, and
drives `scripts/agent-pilot.py`. It loads `scripts/pilot/acl.toml` — a local policy that names
the three pilot principals — so the committed `acl.toml` and the hosted deployment's policy
are unchanged. The pilot principals are throwaway deterministic secrets; no deployment
configuration is touched.

**It creates four repositories** in the Neon branch behind `.env.local`, and there is still no
repository deletion endpoint, so see "Cleanup" below.

## Result — 2026-10-17, PASS 20/20

```text
PASS  identity: /auth/whoami names agent-a/b/c
INFO  isolation-today: agent-a read agent-b's repo refs (HTTP 200) — expected gap
PASS  independent: each of three agents advanced its own repository by 3
PASS  independent: each repository tree carries all three paths
PASS  shared-ref: all three disjoint pushes landed
PASS  shared-ref: final tree carries every agent's file
PASS  shared-ref: the ref advanced once per landing
PASS  contention: exactly one same-path push landed
PASS  contention: the other was a clear 409, not a lost ref
PASS  contention: the ref carries the winner's bytes
PASS  recovery: the conflicted agent landed on the winner's tip
PASS  recovery: the loser's content is now the ref content
PASS  workspaces: agent-a/b/c each submitted a changeset
OK  20 checks passed across 3 agents and 4 repositories
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
was rejected for losing the race** — the same-ref, disjoint-path case is collision-free.

The same-path case is the honest counterexample: one push commits, the other returns
`409 conflict`, the ref keeps the winner's bytes, and the loser recovers by reading the new
tip and landing on it. That is serialization with a clear error, not silent corruption.

## What this proves

- Per-agent identity: each principal mints its own token and `/auth/whoami` names it.
- Concurrent independent repositories: three agents' pushes interleave with no cross-repo errors.
- Same-ref, disjoint paths: all land; Land's automatic rebase absorbs the contention.
- Same-ref, same path: exactly one winner, a clear 409 for the loser, no lost ref.
- Retry recovery: the loser can re-read the ref and land, no half-applied state.
- Concurrent workspaces: three workspaces created, written and submitted in parallel.

## What this does not prove

- **Per-repository isolation.** `acl.toml` is path/ref-scoped, not repo-scoped: the policy does
  not see the repository name. agent-a reading agent-b's repository is recorded as INFO, and it
  is the M1 P0 gap from `docs/M1-readiness-and-multi-agent.md`, not a pilot regression.
- **Per-agent write scopes on `push`.** Scion already enforces write on every changeset delta
  path inside `create_changeset` (`PolicyWriteAuthz`), but the façade's `push` first requires
  write on `/` (`crates/forgevcs-service/src/facade.rs`, the `authorize(... Action::Write,
  Some("/") ...)` before uploads). Requiring write on `/` means every agent needs write
  everywhere, so a per-path pilot policy cannot restrict pushes yet. Relaxing that coarse gate
  — and relying on the per-path check the engine already runs — is the enabling change for
  per-agent write scopes.
- **Explicit merge/rebase resolution.** `Land` rebases contended changesets internally, and the
  vendored Scion server has `WriteService.Rebase` and `WriteService.Merge` handlers, but the
  façade exposes neither. Agents can retry onto a new tip; they cannot name a merge.
- **Hosted/multi-instance behavior, deletion, rate limits, soak.** This is one local process on
  one Neon branch. Eight-hour, ten-agent behavior needs the load/soak work and the hosted run.

## Findings worth carrying into M1

1. Rebase-on-land is real and already handles the common multi-agent case. The gap is not
   "concurrent pushes fail"; it is "the losing agent has no API to resolve a content conflict".
2. The policy scope is the blocker for isolation, not the token layer: identities are already
   per-agent, and principals are named in policy independently of the secret rotation.
3. The façade's coarse `write on /` gate on `push` currently overrides the finer per-path
   authorization the engine already performs. Changing that gate is a small, testable step.
4. No deletion endpoint means every pilot run is permanent. This run added eight repositories
   (see below); the deletion workstream is a prerequisite for routine piloting.

## Cleanup

Created by this workstation's pilot runs on 2026-10-17 (no deletion endpoint; remove with SQL
against the Neon branch):

- `agent-pilot-2610171727059d7e-a`, `-b`, `-c`, `-shared` — first run, stopped at the recovery check
- `agent-pilot-261017172902fd75-a`, `-b`, `-c`, `-shared` — passing run

Repositories are rows in the Scion metadata tables plus object keys in the bucket; deleting one
means every Scion table row for its `repo_id` plus the object keys (the same reviewed
destructive path `docs/W4-engine-wiring.md` §8.6 records as absent).

## Next

1. Expose `Rebase` and `Merge` from the façade (the upstream handlers exist) so a conflicted
   agent has a designed path instead of "retry onto the new tip".
2. Relax the `push` write-on-`/` gate to the delta paths the engine already checks, then add a
   pilot policy with per-agent write prefixes and prove the deny in this harness.
3. Repeat this harness at N agents for a bounded soak and record error rate/latency; then run
   it against the hosted deployment for the multi-instance property.
4. Build repository deletion before the pilot count grows further.
