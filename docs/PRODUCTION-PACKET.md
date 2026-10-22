# ForgeVCS production packet

Status: prioritized work packet. **Updated 2026-10-22** (Lane R backup audit + Lane Q read-only
review; committed state `b31f61a` — a concurrent uncommitted working-tree change is starting the
metrics/merge-engine fixes and is not claimed here) from `docs/M1-readiness-and-multi-agent.md`, `docs/M1-agent-pilot.md`,
`docs/M1-merge-analysis.md`, `docs/TECH_DEBT.md`, `docs/M1-repo-acl-and-path-scopes.md`,
`docs/R4-workspace-lease.md`, `docs/MIRRORING.md`, and live hosted checks. The current truth and
the remaining work by owner/acceptance/rollback live in **`docs/PRIMARY-VCS-DOCKET.md`**; this
packet is the ordered list, and it supersedes the 2026-10-18 status statements below it.

## Verdict (2026-10-22)

ForgeVCS is **controlled-pilot ready and in use as the mirror/landing home for agent lanes, not
prime-time primary-VCS ready.** Hosted `GET /health` and `GET /ready` both answer **200**. The VCS
core is green: repositories/push/pull/refs/changesets, workspaces with a write lease, repository
ACL + path scopes, conflict capture with resolve/continue, deletion/DevOps authority, human CLI and
the one-way mirror are all shipped. What is missing is **operational evidence**, not another VCS
feature:

1. **Metrics are disconnected** — `/metrics` cannot report real activity (P0-6 below).
2. **No N-agent soak/scale run** — only single-process pilots (P0-4 below).

Backups were re-scoped by the principal on 2026-10-22: a **local zip pushed to iCloud is the
offsite copy**, with a **Neon object-storage backup branch as the fallback**. No offsite-upload code
is required for that decision (see P0-5).

## P0 — required before any multi-agent production use

| # | Item | Status (2026-10-22) |
| --- | --- | --- |
| P0-1 | Repository-scoped authorization (isolation) | **DONE** — `crates/forgevcs-service/src/repo_acl.rs`; owner claim + configured readers/writers + admin bypass; live 18-check proof in `docs/M1-repo-acl-and-path-scopes.md` |
| P0-2 | Path-prefix write scopes on push | **DONE** — per-delta path authorization in the façade; checked-in policy narrowed to lane prefixes (R3, 2026-10-19); same 18-check proof |
| P0-3 | Rate limiting / quotas | **MOVED TO DEBT** (TD-4); replaced by the wall-time check in Culebraluxe `docs/agent/RUN-BUDGET.md` |
| P0-4 | N-agent soak with real agents | **OPEN — top blocker** (with P0-6). Three-agent and real-work merge pilots exist (R5, 33/33) but they are single-process; no 10-agent concurrent/retry/scale evidence |
| P0-5 | Offsite backups + scheduled restore proof | **ACCEPTED / RE-SCOPED (principal, 2026-10-22)** — local zip pushed to iCloud is the offsite copy; Neon object-storage backup branch is the fallback. Local zip + weekly restore drill + failure alert are shipped (TD-2/TD-3). No offsite-upload code is required |
| P0-6 | Operational metrics connected | **OPEN — top blocker.** `Metrics::new()` is constructed fresh in `crates/forgevcs-service/src/http.rs:151` while engines hold their own metrics (`engine.rs:140`); the endpoint reports all-zero/empty values (e.g. the land histogram) and there are no conflict metrics, dashboards, or alert rules |

### P0-4 — N-agent soak (acceptance)

10 agents, independent repositories and workspaces, sequential + concurrent pushes, rename and
conflict paths, retry behaviour on 503/409. Record: lost refs = **0**, error rate, p50/p99, retry
counts, half-applied state = none. Deliver a written report + artifacts (a one-off script and its
output, not a new long-lived service). Depends on P0-6 so the run has real metrics to report.

### P0-6 — metrics connected (acceptance)

- `/metrics` reads the **same** metric registry the engines record into (per-engine handles or one
  shared registry), proven by a load call showing non-zero land/conflict counters.
- Conflict counters: paused/resolved/continued/refused, by repo/ref.
- Alert rules: readiness, error rate, DB saturation; one dashboard an operator actually opens.
- Owner: service + DevOps. Rollback: keep the new endpoint behind the existing route; if the shared
  registry regresses, revert to the previous handler (metrics endpoint is read-only).

## P1 — production posture (after P0)

1. **Docs truth pass** — `docs/M1-readiness-and-multi-agent.md` and `docs/M1-merge-analysis.md` still
   describe P0-1/P0-2/R2/R3/R4/R6, the human CLI and mirroring as gaps; the X-2 material
   (Culebraluxe `docs/agent/FORGE-CUT-LIST.md`) is similarly stale. Status banners point here now;
   the full reconciliation is tracked in the docket (P1-A).
2. **Merge engine determinism** — `FORGEVCS_MERGE_ENGINE` is read directly inside vendored Scion
   (`upstream/crates/scion-history/src/rename.rs:36`), default `default` (delete+add rename). At
   committed HEAD (`b31f61a`) the service neither validates nor reports it on `/version`, and it is
   **not recorded in repository history**, so the same operation can produce different changeset
   identities under a different deployment config. **Working tree 2026-10-22 (Lane W):** the value
   is validated (`default|exact-rename-v1`, invalid fails `/ready`), reported on `/version` and in
   the push/land/merge-land responses, and logged on every successful landing — see
   [`docs/MERGE-ENGINE.md`](MERGE-ENGINE.md). Recording it in generated history is blocked upstream
   (exact blocker in that doc). Acceptance: validate the env value, report it on `/version` and in
   the push/land response, record it in generated history or an audit row, and decide the default.
3. **Two-parent landing / lane-to-lane topology** — an explicit product decision, not an engine
   default. Merge-aware policy must not hide conflicts on write-but-not-read paths (a ref visible to
   an agent while the conflict material is not); decide the topology and test the policy surface.
   Recommendation and migration path: [`docs/TOPOLOGY-DECISION.md`](TOPOLOGY-DECISION.md)
   (squash-only now, reopen on a concrete topology trigger). **Working tree 2026-10-22 (Lane Y):**
   the write-but-not-read conflict case is now fail-closed in the vendored filter (typed
   `permission_denied`, no partial report/path leak); read-only omissions and clean entries keep
   the old behaviour — see `docs/TECH_DEBT.md` TD-12.
4. **X-2 cut** — dependency-complete (`cargo tree` has no `forge` in the app) but the parity/
   transition test was skipped; Culebraluxe tests pin Forge `v0.2.2` while the Forge checkout is
   `v0.2.5`; the app still owns/writes Forge schema (X-3). Close-out plan (golden payload/audit
   parity, pin bump, X-3 ownership): Culebraluxe
   `docs/agent/X-2-CLOSEOUT.md`. Acceptance: parity/transition test green, pin moved, X-3 ownership
   decided in `FORGE-CUT-LIST.md`.
5. **Human login + secret rotation** — human CLI is shipped; the browser/GitHub session path,
   per-agent revocation/rotation without redeploy, and per-repo policy admin remain. (2–3 days)
6. **Staging/production separation + CI** — a second Vercel environment and a checked pipeline for
   service suite/gate/smoke/deploy. (1 day)
7. **Retention/purge** — `forgevcs_auth_decisions` pruning exists (`scripts/prune-auth-decisions.sh`,
   `docs/AUTH-DECISION-RETENTION.md`); confirm the ordered migration runner/ledger covers
   multi-instance rollout before calling it closed. Deleted-history object lifecycle remains.
8. **MCP read/write surface** — agents call tools directly, not only HTTP. (2 days)
9. **Git wire protocol** — evaluate Git-compatible clone/push or keep the explicit mirror/import
   path; not required for the agent lanes.
10. **Mirror cutover** — git remains the source while work happens (`docs/MIRRORING.md`);
    `public/` is excluded (126 binaries, 66 MB) and the drift alarm (`scripts/mirror-all.sh --check`,
    exit 3) is operator-run, not scheduled. Decide the cutover/schedule before "primary VCS" is
    claimed for humans.

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

All P0 closed with tests/evidence — including **connected metrics** and a **10-agent soak with zero
lost refs**; the backup offsite decision recorded and drilled (local zip → iCloud, Neon branch
fallback); staging environment + CI green; human login and per-repo ACLs in place; the two-parent/
lane topology and merge-engine recording decided. Then ForgeVCS can be the primary VCS for the
agent lanes (not necessarily for human Git users).
