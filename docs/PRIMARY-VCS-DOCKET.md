# PRIMARY-VCS-DOCKET — ForgeVCS as the agent lanes' primary VCS

**Status:** working docket, 2026-10-22 · **Tree:** committed `b31f61a` (`docs/PRIMARY-VCS-DOCKET.md`
adds no code) · **Author:** Engineer (Lane R) · **Sources:** Lane Q read-only review (2026-10-22, folded
below and attributed), this repository's docs (`PRODUCTION-PACKET.md`,
`M1-readiness-and-multi-agent.md`, `M1-agent-pilot.md`, `M1-merge-analysis.md`,
`M1-repo-acl-and-path-scopes.md`, `R4-workspace-lease.md`, `MIRRORING.md`, `TECH_DEBT.md`,
`AUTH-DECISION-RETENTION.md`), and live read-only checks.

**What this docket is:** the current truth about whether ForgeVCS can be the primary VCS for the
agent lanes, separated into **done**, **deliberate debt**, and **real blockers**, with owner,
acceptance evidence and rollback for each remaining item. It is not a re-run of Lane Q's review and
it is not a production change: no production database writes, no deploy, no commits.

**Working-tree note (2026-10-22):** at the time of writing, uncommitted changes in
`crates/forgevcs-service` from a concurrent lane were adding a shared metrics registry with conflict
counters and `FORGEVCS_MERGE_ENGINE` validation/`/version` reporting. They are **not committed,
not reviewed by this docket, and not claimed as done below**; B1/P1-B describe committed `b31f61a`.

## Verdict

**Controlled-pilot ready and in use as the mirror/landing home for agent lanes; NOT yet prime-time
primary-VCS ready.** The VCS core is not the problem — repositories, push/pull, changesets,
workspaces with a real write lease, repository ACL + path scopes, conflict capture with
resolve/continue, deletion, DevOps authority, human CLI and the mirror are shipped, and the hosted
service answers `GET /health` **200** and `GET /ready` **200**. The two blockers are operational
evidence:

1. **Metrics are disconnected** — the fleet cannot observe land/conflict activity.
2. **No N-agent soak/scale run** — only single-process pilots exist.

Until those close, "primary VCS" would be a claim about intent, not about demonstrated operation.
The backup item (P0-5) is **not** a blocker any more: the principal re-scoped it on 2026-10-22 to a
local zip pushed to iCloud, with a Neon object-storage backup branch as the fallback (no
offsite-upload code required).

## 1. Done (shipped, with evidence)

| Area | State | Evidence |
| --- | --- | --- |
| Engine + service + hosted deployment | W0–W8/M0 complete; hosted `/health`, `/ready` 200 today | `docs/W*.md`, `docs/M1-readiness-and-multi-agent.md`; live check below |
| Repository lifecycle | init/push/pull/refs/changesets/file/tree/diff/merge-base; DevOps-only deletion + queue clear | `crates/forgevcs-service/src/facade/`, `docs/M1-roles.md` |
| P0-1 repository-scoped ACL | **DONE** — owner claim, configured readers/writers, admin bypass, default deny | `crates/forgevcs-service/src/repo_acl.rs`; `docs/M1-repo-acl-and-path-scopes.md` (live 18-check proof) |
| P0-2 path-prefix push scopes | **DONE** — per-delta path authorization; coarse gate only for repo/ref provisioning | same doc; `scripts/lane-scope-acceptance.sh` |
| R2 conflict workflow | **DONE** — paused conflict persisted; `GET /repos/{repo}/conflicts`, `GET/POST /conflicts/{id}/resolve|continue`; `forgevcs conflicts` CLI | `crates/forgevcs-service/src/{paused_conflicts.rs,facade/}`, `docs/M1-merge-analysis.md` §2c |
| R3 lane-prefix policy rollout | **DONE** — checked-in `acl.toml` narrowed to lane prefixes 2026-10-19 | `docs/M1-repo-acl-and-path-scopes.md` §R3; `scripts/pilot/acl-lanes.toml` |
| R4 workspace write lease | **DONE** — DB lease + fencing token, typed 409 for a second writer | `crates/forgevcs-service/src/lease.rs`, `docs/R4-workspace-lease.md` |
| R5 real-work merge pilot | **DONE** (per Culebraluxe `docs/agent/REVIEW-ACTION-PLAN.md`: 33/33 assertions, deliberate conflict resolved, tree digest matched jj) | Lane Q review; Culebraluxe repo (outside this checkout) |
| R6 retention / ordered migration ledger | **Shipped** (Lane Q): auth-decision pruning + schema-migration ledger | `docs/AUTH-DECISION-RETENTION.md`, `scripts/prune-auth-decisions.sh`, `crates/forgevcs-service/src/db.rs` |
| Human CLI | `forgevcs` operator CLI (repo/ref/workspace/conflict commands) | `crates/forgevcs-cli/`, `docs/WORK-ORDER-HUMAN-CLI.md` |
| Mirroring | Three repos mirrored; `--check` drift alarm exits 3 | `docs/MIRRORING.md`, `scripts/mirror-all.sh`, commits `wire-1`/`wire-2` |
| Backups (local copy) | Daily verified zip per repository + retention + failure alert; weekly unattended restore drill with JSON evidence | `scripts/scheduled-backup.sh`, `scripts/restore-drill.sh`, `scripts/backup_acceptance.py`, `docs/TECH_DEBT.md` TD-2/TD-3 |

## 2. Real blockers (ordered)

### B1 — P0: operational metrics are disconnected

**Finding (Lane Q, confirmed by code read):** `crates/forgevcs-service/src/http.rs:151` builds a
fresh `Metrics::new()` for the HTTP state while each engine keeps its own registry
(`crates/forgevcs-service/src/engine.rs:140`). `/metrics` therefore cannot report what the engines
did: the land histogram is all-zero, and there are no conflict metrics, dashboards, or alert rules.

**Consequence:** an operator cannot tell a healthy idle system from a broken busy one; the soak test
(B2) would produce no usable latency/error evidence; SLOs and on-call are impossible.

**Acceptance:** `/metrics` reads the same registry the engines record into (shared handle or
per-engine aggregation); a load call shows non-zero land/conflict counters; conflict counters
(paused/resolved/continued/refused by repo/ref); alert rules for readiness, error rate and DB
saturation; one dashboard. **Owner:** service + DevOps. **Rollback:** the metrics route is
read-only — revert the handler change alone.

### B2 — P0: no N-agent soak/scale evidence

**Finding (Lane Q):** the packet's P0-4 is still open. Existing evidence is a 3-agent
single-process pilot (`docs/M1-agent-pilot.md`) and the real-work merge pilot (R5, 33/33), not a
10-agent concurrent/retry/scale run.

**Consequence:** the prime-time claim is unsupported; Vercel scale-in/out behaviour, 503/409 retry
idempotency and lost-ref risk under load are untested.

**Acceptance:** 10 agents, independent repositories/workspaces, sequential + concurrent pushes,
rename and conflict paths, retry behaviour. Record lost refs = 0, error rate, p50/p99, retry counts,
half-applied state = none; written report + artifacts. **Owner:** DevOps + QA, after B1 so the
metrics are real. **Rollback:** none needed (a test, not a deployment) — the pilot repositories must
be deleted in teardown.

### P0-5 — backup/offsite: decided, not a blocker (recorded here so it is not reopened by accident)

**Principal decision (2026-10-22):** a **local zip pushed to iCloud is the offsite copy**; a **Neon
object-storage backup branch is the fallback**. The shipped local zip + weekly restore drill +
failure alert remain the mechanism. No offsite-upload code is required, and none is integrated.

**Residual risk (accepted):** the iCloud push is an operator step, not automated; the weekly drill
proves the local archive, not the iCloud copy; evidence is local. **Reopen only if** the principal
wants automated offsite copies — in that case the safe shape is: verified local zip → object/branch
copy → fetched-and-restored proof, and the object must be deleted after the proof. **Owner of the
decision:** principal; **owner of the manual push:** DevOps.

> Note for the next engineer: an offsite-upload prototype (S3 SigV4 helper + drill phase) was built
> and proven against the dev branch (`forgevcs-ci`, bucket `forgevcs-objects`) during this lane, then
> stopped by the principal. The helper is **uncommitted and unintegrated** at
> `scripts/backup-offsite.sh`; the backup scripts were reverted to their committed state. Do not
> wire it in without a new decision.

## 3. Deliberate debt (accepted; not blockers)

| Item | Why accepted | Closing condition |
| --- | --- | --- |
| Rate limits / per-agent quotas (TD-4) | free models; no spend worth gating | paid models or shared production load returns |
| `psql` required for archive metadata (TD-2) | operator tool, loud failure, verified archives | store-backed metadata export endpoint |
| Backup offsite is a manual iCloud push (TD-3) | principal decision 2026-10-22 | principal reopens automation |
| Mirror is one-way; git remains source; `public/` excluded (126 binaries / 66 MB); drift alarm is operator-run | cutover not yet wanted | schedule `mirror-all.sh --check` and define the source-of-truth cutover |
| No Git wire protocol | REST façade is enough for agents | only if Forge must replace Git for humans |
| Two-parent landing / lane-to-lane is not a default topology | explicit product decision, not engine default | see P1-C below when lanes need convergence |
| Merge engine default is upstream behaviour | safe default, compatible with existing history | see P1-B |

## 4. P1 — posture work (owner, acceptance)

| # | Item | Owner | Acceptance |
| --- | --- | --- | --- |
| P1-A | **Docs truth pass.** `docs/M1-readiness-and-multi-agent.md`, `docs/M1-merge-analysis.md` and the X-2 material (Culebraluxe `docs/agent/FORGE-CUT-LIST.md`) still present shipped work as gaps (P0-1/P0-2/R2/R3/R4/R6, human CLI, mirror). Status banners now point here; full reconciliation pending | Lead/Engineer | each doc either updated or marked superseded, with the shipped evidence linked |
| P1-B | **Merge engine determinism.** `FORGEVCS_MERGE_ENGINE` is read directly inside vendored Scion (`upstream/crates/scion-history/src/rename.rs:36`), default `default`; at committed HEAD the service does not validate or report it and it is not recorded in repository history, so the same operation can yield different changeset identities across deployments. Working tree 2026-10-22 adds validation + `/version` + push/land response/log evidence; history recording is blocked upstream — details in ForgeVCS `docs/MERGE-ENGINE.md` | service | env value validated; reported on `/version` and in push/land responses; recorded in generated history or an audit row; default decided; a changeset id is reproducible from recorded inputs |
| P1-C | **Two-parent landing / lane-to-lane topology + merge-aware policy.** Merge-aware policy can hide conflicts on write-but-not-read paths (a ref visible while its conflict material is not). Recommendation + migration path: ForgeVCS `docs/TOPOLOGY-DECISION.md` | Lead + service | topology decision written; policy test covers read vs write of conflict material; no silently hidden conflict |
| P1-D | **X-2 cut parity.** dependency-complete (`cargo tree` has no `forge` in the app) but the parity/transition test was skipped; Culebraluxe tests pin Forge `v0.2.2` while the checkout is `v0.2.5`; app still owns/writes Forge schema (X-3). Close-out plan: Culebraluxe `docs/agent/X-2-CLOSEOUT.md` | Lead + app/Forge | parity/transition test green; pin moved; X-3 ownership written in `FORGE-CUT-LIST.md` |
| P1-E | **Human login + secret rotation** (browser/GitHub session, per-agent revocation, per-repo policy admin) | service | hostile-path tests for session, revocation without redeploy, per-repo admin |
| P1-F | **Staging/production separation + CI** (second Vercel environment, checked gate/smoke/deploy pipeline) | DevOps | a deploy cannot bypass the gate; staging run recorded |
| P1-G | **SLOs/alerting** (readiness, error rate, latency, DB saturation; on-call runbook) | DevOps | alerts fire on an induced failure; runbook linked. Depends on B1 |
| P1-H | **Retention/purge completeness** (auth decisions shipped; deleted-history object lifecycle; ordered migration ledger across instances) | engine + DevOps | prune job evidenced; lifecycle policy applied; migration ledger matches applied set |
| P1-I | **MCP read/write surface** | service | an agent completes one read and one write through MCP without HTTP glue |

## 5. Acceptance evidence recorded for this docket (read-only)

```text
$ curl -sS -o /dev/null -w 'health=%{http_code} ready=' https://culebraluxe-forgevcs.vercel.app/health
health=200 ready=200

$ cargo test -q -p forgevcs-service --lib --bins
test result: ok. 97 passed; 0 failed; 0 ignored

$ cargo test -q -p forgevcs-cli
test result: ok. 8 passed; 0 failed; 0 ignored

$ scripts/gate-status.sh
pass: 1348 passed, 0 failed (3707276 on main) — ~/Library/Logs/forgevcs/gate-20261019-035811.log
```

The service/CLI test runs were made during this audit; a concurrent lane's uncommitted changes
appeared in the working tree after those runs and are not part of these counts.

Lane Q's review is the source for B1/B2 and P1-A…P1-D; this docket did not re-run that read-only
review. The backup prototype's object-storage proof (dev branch only) was deleted; a list of the
prototype prefix returned **no objects**, and no production database was written.

## 6. Rollback

- **This docket / packet:** documentation only; revert the docs commit.
- **B1 metrics fix (when made):** read-only endpoint change; revert the handler and keep the old
  `/metrics` response shape.
- **P1-B merge-engine recording:** additive audit/history field; revert the recording change without
  changing merge behaviour (engine selection remains the env default).
- **Backup decision:** nothing to roll back — no offsite code is integrated. The uncommitted
  prototype helper can be deleted (`rm scripts/backup-offsite.sh`) or left unintegrated; enabling
  offsite later needs a new decision and the safe shape described in §2 P0-5.
- **Soak (P1 evidence work):** throwaway repositories are deleted in teardown; residue is reported
  rather than left in the control plane.

## 7. Out of scope / not verified here

- No production database writes, no deploy, no VCS commit was made by Lane R.
- No N-agent soak was run (that is B2's work, not a documentation claim).
- Lane Q's read-only findings are folded as findings, not re-audited line by line.
- Culebraluxe-repo documents (X-2/X-3 material) are flagged, not edited from this checkout.

## Update — 2026-10-22 execution

The stale snapshot above is superseded by these committed/deployed facts:

- **B1 service fix:** shared `MetricsHub` is live; `/metrics` exposes one storage family and
  land/conflict counters (commits `ba3fe9a`, `ef47e45`). **Hosted scrape/dashboard/alerts remain
  open** (TD-6), so the observability blocker is no longer the endpoint.
- **B2 soak:** 10-agent, 10-repo, 11-process soak **PASS** — 712 attempts, 0 lost refs, 0 partial
  writes, p99 25.81 ms (`7f1831a`, `docs/SOAK-2026-10-22.md`). The soak also found the
  second-engine duplicate-metrics 503, fixed in `ef47e45` with a live two-engine test.
  Multi-repo-per-process boot is safe; a full 10-repo single-process soak was not rerun.
- **Deployment:** `ef47e45` is live with `FORGEVCS_MERGE_ENGINE=exact-rename-v1`; W8 hosted
  acceptance passed W4 20/20 and W5 17/17 (`docs/W7-vercel-deployment.md` receipt).
- **P1-A** docs truth pass done; **P1-B** validation/reporting done, history recording still
  upstream-blocked; **P1-C** squash-only memo done; **P1-D** X-2 close-out plan done, execution
  pending.
- **Primary verdict:** ForgeVCS is deployable and proven for the process-per-repo agent-lane
  topology. It is not yet the sole production VCS until hosted observability alerts exist and the
  optional single-process 10-agent soak is run.
