# After M0 — readiness and open work

**Date:** 2026-10-17 · **Tree:** `c69408a` (`m0`, `w7-local-build`)

## Verdict

**ForgeVCS is M0-complete and suitable for a controlled pilot. It is not prime-time / primary-VCS
ready.** It can host repositories, push and read changesets, run disposable workspaces and serve
them from Vercel with Neon persistence. It does not yet have the identity, data-lifecycle,
collaboration and operational controls that a team's primary source-control system needs.

Use it now for:

- disposable or non-critical repositories;
- machine/service principals, one active writer per workspace;
- source-control operations through the REST façade;
- pilots that expect no repository deletion, no merge/rebase UI, and no human login.

Do not use it yet as the only copy of important history, as a team's Git replacement, or with
multiple agents writing the same workspace.

## Blockers before prime usage

### Identity and authorization

| Gap | Consequence | Priority |
| --- | --- | --- |
| Human login is not exposed | Only `/auth/token` service principals can use the service; no browser/GitHub session path | P0 |
| Culebra has no service-principal identities | Hosted mode is `SCION_ONLY`; enabling `DUAL_ENFORCE` would deny every agent | P0 |
| `acl.toml` puts all agents in one `agents` group | No per-agent, per-team or per-repository isolation; every configured agent can read/write every repository | P0 |
| One shared signing/principal configuration per deployment | No per-agent quota, revocation or rotation workflow; rotating an agent secret is an env edit + redeploy | P1 |
| No repository-scoped policy administration | Creating a repo does not create/assign an owner or ACL | P1 |

### Data safety and lifecycle

| Gap | Consequence | Priority |
| --- | --- | --- |
| No repository deletion | Smoke/test repos accumulate; a compromised repo cannot be removed through the API | P0 |
| No tested backup/restore procedure | Neon branch restore exists as a platform action, but there is no ForgeVCS-level restore drill | P0 |
| No retention/purge for `forgevcs_auth_decisions` | Append-only table grows with every request in shadow/dual modes | P1 |
| Schema is runtime DDL + two migration files | No migration runner, no ordered rollout ledger for multiple instances | P1 |
| No object-storage lifecycle/GC for deleted history | Storage grows forever; no repo-level accounting | P1 |

### Collaboration

| Gap | Consequence | Priority |
| --- | --- | --- |
| No merge/rebase/resolve REST endpoints | Agents can only work on separate refs/repos; converging work is manual or out of band | P0 for team use |
| No review/PR/comments/diff discussion | No collaboration review surface; the façade is a plumbing API | P1 |
| Workspace writes are last-writer-wins across instances | Two agents editing one workspace can silently overwrite each other; no distributed lock | P1 |
| Paused rebases are not fully restored | Rebase conflict materialization is not replayed from the persisted overlay | P2 |
| No workspace listing or quotas | No way to find/limit active workspaces from the API | P1 |
| Ref protection is group-wide, not per-branch/per-actor | No “main requires review” or “agent X may only push topic branches” | P1 |

### Operations

| Gap | Consequence | Priority |
| --- | --- | --- |
| No app-level rate limiting or per-agent quotas | One agent can consume the pool/connection budget | P0 |
| No load, soak or chaos test | Multi-agent concurrency and Vercel scale-in are only smoke-tested | P0 |
| No SLO/alerting on readiness, error rate, latency or DB saturation | Failures are discovered by a client, not an operator | P1 |
| No custom domain; `*.vercel.app` is behind Deployment Protection | External agents need the automation-bypass header; no stable public name | P1 |
| No Git integration/preview deployments | CLI release only; no push-to-deploy, no PR previews | P2 |
| One Vercel project/one environment | No staging/production separation for ForgeVCS itself | P1 |
| No CI workflow for the service suite | Gate/smoke/deploy are operator commands, not a checked pipeline | P1 |

### Interoperability

| Gap | Consequence | Priority |
| --- | --- | --- |
| REST façade only, not Git wire protocol | `git clone`/`git push` do not work; clients must use ForgeVCS's API | P0 for Git replacement |
| No Git mirror/import/export | Existing Git repositories cannot be synchronized | P1 |
| No MCP read/write surface | Agents cannot use MCP tools directly; they need HTTP clients | M1 |
| No Forge SDLC/review-service integration | ForgeVCS is not yet the engine behind the Forge pipeline | M1 |

## What already works

- Hosted service: `/health`, `/ready`, `/version`, service-principal tokens, `/auth/whoami`.
- Repositories: `init`, `push`, `pull`, refs, commits, changesets, file/tree, diff, merge-base.
- Workspaces: create/materialize/status/read/write/delete/diff/submit/destroy; persisted state
  and dirty overlay; restore across Vercel instances.
- Authorization A/B gate: `SCION_ONLY` (hosted), `SHADOW_COMPARE`, `DUAL_ENFORCE`; persisted
  decisions.
- Release: local linux/amd64 compile, runtime-only Vercel image (~9 s platform build), hosted W8
  smoke W4 20/20 + W5 17/17 twice.
- Verification: upstream gate 1343/0, service 34 tests, clippy/fmt clean, Neon metadata and
  object-storage conformance.

## Multi-agent pilot — recommended next test

The service already supports multiple principals (`FORGEVCS_SERVICE_PRINCIPALS` is a comma-separated
`name=<sha256>` list), but `acl.toml` must name each principal in `agents`. A useful pilot is
**independent agents on separate repositories/workspaces**, not one shared workspace.

### Topology

```text
agent-a ── service:agent-a ── repo/pilot-a ── workspace A
agent-b ── service:agent-b ── repo/pilot-b ── workspace B
agent-c ── service:agent-c ── repo/pilot-c ── workspace C
                  │
                  └── same ForgeVCS project + Neon branch
```

Each agent gets its own secret/hash and its own repository prefix. No agent shares a workspace.

### Checks

1. **Identity:** each principal mints a token; `/auth/whoami` reports its own name; wrong secret 401.
2. **Isolation today:** agent A *can* read repo B (all agents share one group). Record this as the
   expected current behavior and the reason per-repo ACLs are a P0 gap.
3. **Ref CAS:** two agents push different children from the same parent to one ref; exactly one
   advances, the other gets a conflict/permission error; no lost ref.
4. **Cross-instance workspaces:** agent writes, next request lands on another instance, status/file
   still see the overlay (the W8 property).
5. **Separate-workspace concurrency:** N agents push/read in parallel; measure error rate and
   latency; no cross-repo interference.
6. **Retry behavior:** agents retry 503/409 responses; confirm operations are idempotent or clearly
   rejected, not partially applied.
7. **Cleanup:** because repository deletion is absent, count the rows/repos created and plan a
   manual Neon cleanup after the pilot.

### What the pilot can and cannot prove

- It can prove multi-principal auth, independent-repo concurrency, persisted workspaces under
  Vercel routing, and ref CAS.
- It cannot prove same-repository collaboration: there is no merge/rebase/review API and no
  per-agent branch policy.
- Do not let two agents write the same workspace: the last writer wins across instances.

## Suggested next workstream (M1)

1. **Repository lifecycle:** delete/archive, ownership, per-repo ACL, backup/restore drill.
2. **Collaboration:** merge/rebase/resolve endpoints, per-ref policy, review service.
3. **Identity:** human login, per-agent Culebra identities, secret rotation/revocation.
4. **Operations:** rate limits, quotas, staging project, CI, SLOs, retention.
5. **Interop:** Git mirror/import, MCP read/write, Forge integration.

A multi-agent pilot should start only after (or alongside) the identity/isolation and deletion
work; otherwise it tests agents sharing one permission group with no cleanup path.
