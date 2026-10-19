# ForgeVCS production packet

Status: prioritized work packet (2026-10-18), derived from `docs/M1-readiness-and-multi-agent.md`,
`docs/M1-agent-pilot.md`, `docs/TECH_DEBT.md`, and the passing service suite (38 tests) + upstream
gate. Goal: controlled pilot → production candidate by closing the P0 items in dependency order.

## Verdict

ForgeVCS is **controlled-pilot ready, not prime-time VCS ready**. The engine is green; the
remaining P0s are isolation, abuse limits, and evidence under real concurrency. Sequence below is
deliberate: isolation before load, load before scale, offsite before calling it the only copy.

## P0 — required before any multi-agent production use

### P0-1 Repository-scoped authorization (isolation)
- **Gap:** `acl.toml` is path/ref-scoped, not repo-scoped; agent-a can read agent-b's repositories.
- **Change:** add repository identity to policy evaluation; per-repository ACL/owner; assign on
  `init`; deny cross-repo reads/writes by default.
- **Acceptance:** two agents, two repos: cross-repo read and write refused with typed error;
  same-repo allowed; existing path/ref rules untouched; tests + docs updated.
- **Est:** 1–2 days. **Owner:** Scion/engine + façade.

### P0-2 Path-prefix write scopes on push
- **Gap:** façade `push` requires write on `/`, so every agent needs write everywhere; path scopes
  cannot restrict pushes.
- **Change:** authorize each changeset delta path (the engine already checks finely); keep the
  coarse gate only for repo creation/ref provisioning.
- **Acceptance:** agent with write on `lane/a/**` can push only those paths; write outside refused;
  ref scopes still work; tests.
- **Est:** 0.5–1 day. **Owner:** façade. **Depends:** P0-1 (repo identity in the check).

### P0-3 Rate limiting/quotas — MOVED TO TECH DEBT (2026-10-18)
- **Why:** free models are in use and there is no spend worth gating. Enforcing quotas now is fake rigor.
- **Deferred to:** `docs/TECH_DEBT.md` TD-4. Reopen only if paid models or shared production load return.
- **Replaced by:** wall-time check — `Culebraluxe docs/agent/RUN-BUDGET.md` (flag over N=120 min, not block).

### P0-4 N-agent soak with real agents
- **Gap:** no load/soak/chaos; pilot was one local process, one branch.
- **Change/test:** 10 agents, independent repos/workspaces, sequential + concurrent pushes, rename
  and conflict paths, retry behavior; record lost refs = 0, error rate, p50/p99, 503 retries.
- **Acceptance:** written report + artifacts; any lost ref or half-applied state fails the soak.
- **Est:** 0.5–1 day. **Depends:** P0-1–P0-3 (must be enforced during the test).

### P0-5 Offsite backups + scheduled restore proof
- **Gap:** TD-3: local zip only, no offsite, no scheduled restore spot-check, no failure alert.
- **Change:** ship archives to object storage/Neon branch, run restore drill on schedule, alert on
  non-zero; retention via `BACKUP_RETENTION_DAYS`; remove `psql` dependency (TD-2) or document it.
- **Acceptance:** restore from offsite into a fresh environment, row/object counts checked; a
  failed backup produces an alert; quarterly drill evidence.
- **Est:** 0.5–1 day. **Owner:** DevOps.

## P1 — production posture (after P0)

1. **Conflict resolution workflow** — paused-conflict state + `resolve`/`continue` endpoints;
   today conflicts are read-then-repush by hand. (2–3 days)
2. **Two-parent landing** — harden `merge-land` (re-merge when tip moves) and decide whether
   `POST /land` on a merge head is needed; otherwise document the linear-stack contract. (1 day)
3. **Human login + secret rotation** — browser/GitHub session path, per-agent revocation/rotation
   without redeploy, per-repo policy admin. (2–3 days)
4. **Staging/production separation + CI** — a second Vercel environment and a checked pipeline for
   the service suite (gate/smoke/deploy). (1 day)
5. **SLOs/alerting** — readiness, error rate, latency, DB saturation; on-call runbook. (0.5–1 day)
6. **Retention/purge** — `forgevcs_auth_decisions` and deleted-history object lifecycle. (0.5 day)
7. **MCP read/write surface** — agents call tools directly, not only HTTP. (2 days)
8. **Git wire protocol** — evaluate Git-compatible clone/push or an explicit mirror/import path.
   (large; only if Forge must replace Git for humans)
9. **Forge SDLC integration** — ForgeVCS as the engine behind the Forge pipeline. (M1)

## Explicitly deferred / not required for production candidate

- Directory-level rename identity (file-level is enough for lane flow).
- Both-sides-rename-to-different-path conflict (explicit add/add error is acceptable).
- Recursive merge base (only with lane-to-lane merges or imported Git history).
- Git-compatible wire protocol (REST façade is sufficient for agents).

## Cleanup

Old pilot repositories created before the deletion endpoint may still exist; run the DevOps
deletion/queue-clear path and record residue. The bootstrap default repository is recreated by
design (documented in `docs/M1-roles.md`).

## Definition of production-ready (this packet)

All P0 items closed with tests/evidence; one 10-agent soak run with zero lost refs and an offsite
restore proven; staging environment + CI green; human login and per-repo ACLs in place. Then
ForgeVCS can be the primary VCS for the agent lanes (not necessarily for human Git users).
