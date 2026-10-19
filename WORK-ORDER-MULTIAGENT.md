# ForgeVCS Multi-Agent Readiness — Work Order

**Date:** 2026-10-19  
**Scope:** API, logging, and state management improvements for agent orchestration  
**Total estimate:** ~180–220 engineer-hours (P0–P2)

---

## Executive Summary

ForgeVCS is functionally sound for single-agent use (code review: no bugs). However, multi-agent orchestration requires:
- Structured error signals (agents can't distinguish retryable vs. policy failures)
- Idempotency primitives (agents retry safely)
- State observability (agents see the system before acting)
- Machine-readable logging (agents parse signals)

This work order prioritizes 4 critical systems + 6 polish items, sequenced to unblock agent workflows early.

---

## P0 — Unblocks Agent Decision-Making

### P0.1: Structured Error Codes for `authorize()`

**Problem:** `authorize()` returns bool. Agent can't tell if denied because path doesn't exist, ACL missing, or actual violation.

**Scope:**
- Define error enum: `AuthzError { repo_not_claimed, acl_missing, path_denied, ref_denied, db_unavailable, ... }`
- Refactor `authorize()` → `authorize_checked()` returns `Result<(), AuthzError>`
- Keep `authorize()` as `bool` wrapper for backward compat
- Update `authorize_provisioning()` similarly
- Document each error code with agent retry guidance (retryable: `db_unavailable`; not retryable: `policy_violation`)

**Files:**
- `src/authz.rs` — error enum, new checked variants
- `src/facade.rs` — call sites updated, public API docs
- `docs/AGENT-API.md` — new; error codes + retry strategy table

**Acceptance:**
- [ ] Agent can distinguish "repo not claimed" from "ACL missing" from "denied by policy"
- [ ] All authorize() call sites have guidance (doc comment or code path)
- [ ] Tests cover each error code path
- [ ] Example: agent pseudocode for provisioning workflow (docs)

**Estimate:** 20–24h (error enum, refactor, tests, docs)

---

### P0.2: Idempotency Tokens for `claim()`

**Problem:** Multiple agents racing to claim same repo both attempt INSERT. Winner gets repo; loser gets "conflict" error. No way to tell if you won or lost.

**Scope:**
- Add `claim(repo_id, idempotency_token: UUID, ...) -> {owner: String, claimed_at: Timestamp, is_new_claim: bool}`
- In Postgres: add `claim_token UUID UNIQUE` column to `repos` table
- If same token is submitted twice, return cached result (idempotent)
- If different token tries to claim, return structured error (not `db_unavailable`—genuine conflict)
- Add migration: `migrations/add-claim-token-column.sql`

**Files:**
- `src/repo_acl.rs` — claim() signature + logic
- `migrations/` — new migration
- `src/facade.rs` — expose claim_token in API
- Tests: `tests/repo_acl_idempotency.rs` — retry scenarios

**Acceptance:**
- [ ] Agent can submit claim twice with same token, get same result both times
- [ ] Agent can detect "I won the race" vs. "I lost the race" vs. "error"
- [ ] Migration is reversible; DB schema tested

**Estimate:** 16–20h (schema, logic, migration, tests)

---

### P0.3: State Query API

**Problem:** Agent needs to know current state before acting (is repo claimed? who owns it? what ACLs exist?). Currently must query DB directly or parse logs.

**Scope:**
- Add RPC/endpoint: `GET /repos/{repo_id}/state` 
  - Returns: `{repo_id, claimed: bool, owner: String, acl_entries: [{path, allow/deny, agent_id}], config_version: int}`
- Add RPC: `GET /repos/{repo_id}/claim-status` 
  - Returns: `{claimed: bool, owner: String, claimed_at: Timestamp, claim_token: UUID}`
- Implement in facade.rs, call repo_acl.rs helpers
- Cache with TTL 5s (agents poll at ~1s intervals; cache prevents DB overload)

**Files:**
- `src/facade.rs` — new endpoints
- `src/repo_acl.rs` — new query methods (get_state, get_claim_status)
- `docs/AGENT-API.md` — endpoint specs + examples

**Acceptance:**
- [ ] Agent can query state before attempting claim
- [ ] Agent can poll to detect when claim succeeds
- [ ] Cache hit rate > 80% under typical agent load
- [ ] Example agent workflow (docs): poll-until-claimed pattern

**Estimate:** 24–28h (endpoints, caching, tests, docs, load test)

---

### P0.4: Structured Logging (JSON)

**Problem:** Shell scripts log plaintext. Agents can't parse `"Backup finished in 3.42s"` reliably.

**Scope:**
- Emit JSON-per-line from all service components:
  - `repo_backup.py`: log phases (start, verify, upload, finish) as JSON
  - Shell scripts: replace `echo` with `jq -n` JSON output
  - Rust facade.rs: structured logs at key checkpoints (claim, authorize, push received)
- Fields: `{timestamp, op, repo, status: ok|error, duration_ms, error_code, trace_id}`
- Add log aggregation example (agent can tail and parse)

**Files:**
- `src/facade.rs` — structured log calls (use `tracing` crate if available, else plain JSON)
- `scripts/repo_backup.py` — JSON emit
- `scripts/*.sh` — JSON emit
- `docs/AGENT-LOGGING.md` — log schema, example parser

**Acceptance:**
- [ ] All major operations emit JSON on completion
- [ ] Logs include operation ID (for trace-ability across agents)
- [ ] Agent can parse logs without regex magic (valid JSON per line)
- [ ] Example: simple Python log parser in docs

**Estimate:** 28–32h (retrofit logging, testing, docs, validation)

---

## P1 — Unblocks Idempotent Workflows

### P1.1: Idempotency Tokens for Backup/Restore

**Problem:** If `repo_backup.py` crashes mid-backup, re-running it may corrupt or double-write. No operation ID in manifest.

**Scope:**
- Add operation ID to backup manifest (UUID, set at start)
- In backup.py: before doing work, check if manifest with same op_id exists → skip or resume
- Add checkpoint file: `.forge/checkpoints/{op_id}` tracking progress (start, verify, upload, compress, finish)
- On crash/retry: scan checkpoint, resume from last good step
- Restore similarly: check for resumable checkpoint

**Files:**
- `scripts/repo_backup.py` — operation ID, checkpoint tracking
- Backup manifest schema — add op_id field
- Tests: crash scenario + recovery

**Acceptance:**
- [ ] Backup can be safely re-run if it crashed (no corruption)
- [ ] Restore can resume if interrupted
- [ ] Checkpoint file is JSON (agent-readable)
- [ ] Example: agent orchestration of multi-repo backup (docs)

**Estimate:** 24–28h (checkpoint logic, recovery paths, tests)

---

### P1.2: Explicit Operation Timeouts

**Problem:** Agents don't know how long to wait. Timeouts are implicit (psql connection timeout, etc.) and undocumented.

**Scope:**
- Add `--timeout` flag to all long-running ops: claim, backup, restore, push
- Document default + max values: e.g., claim: 30s (max 5m), backup: 5m (max 1h)
- Return timeout-exceeded error (not generic db_unavailable)
- CLI: `forgevcs claim --repo X --timeout 60s`
- Facade: propagate timeout to backend

**Files:**
- `src/facade.rs` — timeout handling
- `scripts/repo_backup.py` — timeout parameter + enforce
- CLI src — argument parsing
- `docs/AGENT-TIMEOUTS.md` — defaults, tuning guidance

**Acceptance:**
- [ ] Operations respect timeout and return error (not hang)
- [ ] Timeout is configurable per-op, with sensible defaults
- [ ] Tests verify timeout enforcement
- [ ] Docs explain SLA for each op (claim < 30s, backup < 5m for typical repos)

**Estimate:** 16–20h

---

### P1.3: Audit Trail for Authorization Decisions

**Problem:** When an agent grants/denies access, there's no record of *why* (policy version, rule ID, etc.). Makes debugging conflicts hard.

**Scope:**
- Add audit log table: `audit_log (id, timestamp, agent_id, op, repo, decision, policy_version, rule_id, detail_json)`
- On every authorize() call: log the specific rule/policy that matched (or why it didn't)
- Include in audit log: which ACL entry was consulted, path/ref that was checked
- Expose audit log via API: `GET /repos/{repo_id}/audit?limit=100&since=<timestamp>`

**Files:**
- Migrations: `add-audit-log-table.sql`
- `src/authz.rs` — emit audit entries on each decision
- `src/facade.rs` — expose audit query endpoint
- Tests: audit log completeness

**Acceptance:**
- [ ] Every authorize decision is logged with rule ID + policy version
- [ ] Agent can query audit trail to debug conflicts
- [ ] Audit table doesn't become a bottleneck (indexes on repo_id, timestamp)
- [ ] Retention policy documented (e.g., keep 30 days)

**Estimate:** 20–24h

---

## P2 — Operational Polish

### P2.1: Config Dry-Run / Validation Endpoint

**Problem:** If agent wants to deploy a new ACL config, must restart service to validate. No way to pre-validate.

**Scope:**
- Add endpoint: `POST /config/validate` with YAML body → returns `{valid: true, errors: [...]}`
- No changes applied; just checks syntax + policy soundness
- Agent can call before deployment, catch issues early

**Files:**
- `src/config.rs` — extract validation logic (currently at startup)
- `src/facade.rs` — new validation endpoint
- Tests: malformed config rejection

**Acceptance:**
- [ ] Agent can POST new config and get validation feedback
- [ ] Validation catches syntax errors, missing required fields, conflicts
- [ ] No side effects (read-only check)

**Estimate:** 8–12h

---

### P2.2: Composable Authorization Query

**Problem:** Agent calls `authorize_provisioning()` then `authorize()`. If either fails, agent can't tell which layer is blocking.

**Scope:**
- Add: `authorize_with_context(op, repo, path, ref) -> {repo_ok, path_ok, ref_ok, ready}`
- Returns detail on each layer so agent sees the full picture
- Backward compat: `authorize()` still works (calls composed version)

**Files:**
- `src/authz.rs` — add composed variant
- `src/facade.rs` — expose as `/authorize-full` endpoint (opt-in)
- Tests: all combinations of success/failure

**Acceptance:**
- [ ] Agent sees which layer(s) are blocking
- [ ] Helps debug authorization issues faster
- [ ] No perf regression (same DB queries, just richer response)

**Estimate:** 12–16h

---

### P2.3: Timestamp + Sequence Number for Ordering

**Problem:** Postgres race window is safe but opaque. Agent orchestrating multiple repos can't know true ordering of claims.

**Scope:**
- Add `sequence_number` (BIGSERIAL) to repos table
- On claim: return both `claimed_at: Timestamp` and `sequence: BigInt`
- Agent can use sequence to reason about causal order ("repo A claimed before repo B")

**Files:**
- Migrations: `add-sequence-number-to-repos.sql`
- `src/repo_acl.rs` — return sequence on claim
- `src/facade.rs` — expose in claim response
- Tests: ordering correctness

**Acceptance:**
- [ ] Claim response includes sequence number
- [ ] Sequence is monotonically increasing
- [ ] Agent can order events causally using sequence

**Estimate:** 8–12h

---

### P2.4: Connection Pool Observability

**Problem:** If Postgres connection pool exhausts, agents get generic `db_unavailable`. No insight into pool state.

**Scope:**
- Expose metrics: `GET /metrics` (Prometheus-style)
  - `forgevcs_db_pool_size`, `forgevcs_db_pool_in_use`, `forgevcs_db_pool_idle`
  - `forgevcs_auth_checks_total`, `forgevcs_auth_denied_total` (by reason)
- Agent can monitor pool health, adjust concurrency

**Files:**
- Existing metrics crate integration (if available) or add basic metrics struct
- `src/facade.rs` — expose `/metrics` endpoint
- Docs: metrics reference

**Acceptance:**
- [ ] Metrics endpoint returns current pool state
- [ ] Agent can detect pool exhaustion early
- [ ] Metrics include auth decision breakdown

**Estimate:** 12–16h

---

### P2.5: Agent Context Header + Tracing

**Problem:** Logs don't say which agent did what. Multi-agent conflicts hard to debug.

**Scope:**
- Require `X-Agent-ID` header on all requests
- Include in all logs + audit trail
- Add `X-Request-ID` for tracing (trace entire workflow)
- Return request ID in responses so agent can correlate

**Files:**
- `src/facade.rs` — middleware to extract headers
- Logging calls — include agent_id, request_id
- Tests: header handling

**Acceptance:**
- [ ] All logs tagged with agent_id
- [ ] Trace ID propagated through async operations
- [ ] Agent can get request ID from response, grep logs for it

**Estimate:** 12–16h

---

## Dependencies & Sequencing

**Critical path (blocks P1):**
1. P0.1 (Error codes) — unblocks agent decision logic
2. P0.2 (Idempotency tokens) — unblocks safe retries
3. P0.3 (State query) — unblocks agent state checks

**Parallel tracks:**
- P0.4 (Logging) can run alongside P0.1–3
- P1.1–5 depend only on P0.4
- P2.1–5 are independent polish, can run anytime

**Database migrations:**
- Batch: P0.2 (claim_token) + P1.3 (audit log) + P2.3 (sequence) into single migration run
- Test migration reversibility early

---

## Effort & Timeline

| Priority | Items | Hours | Weeks (2 eng) |
|----------|-------|-------|---------------|
| P0       | 1.1, 1.2, 1.3, 1.4 | 88–104 | 4–5 |
| P1       | 1.1–1.5 | 72–96 | 3–5 |
| P2       | 2.1–2.5 | 52–72 | 2–3 |
| **Total** | | **180–220** | **9–13 weeks** |

**Accelerated path (P0 only, then iterate):** 4–5 weeks gets agents unblocked.

---

## Acceptance Criteria (Global)

- [ ] All error codes documented with agent retry guidance
- [ ] All long-running ops support idempotency tokens + timeout
- [ ] State query API allows agents to inspect before acting
- [ ] Logs are JSON, agent-parseable, include tracing context
- [ ] Integration test: multi-agent repo claim (non-deterministic) succeeds safely
- [ ] Load test: 10 concurrent agents, no deadlock or pool exhaustion
- [ ] Docs: agent quickstart guide covering all new APIs

---

## Risk Mitigations

| Risk | Mitigation |
|------|-----------|
| **Postgres schema churn** | Batch migrations; test reversibility; run on staging first |
| **Backward compat breaks** | Keep old authorize() as wrapper; add new variants (_checked, _with_context) |
| **Performance regression from audit logging** | Async audit writes; batch inserts; index on (repo_id, timestamp) |
| **Timeout enforcement complexity** | Start with simple timeout, test thoroughly before async propagation |
| **Multi-agent race conditions** | Comprehensive test suite; idempotency tokens reduce race surface |

---

## Documentation Deliverables

1. **AGENT-API.md** — Error codes, retry strategies, all new endpoints
2. **AGENT-LOGGING.md** — Log schema, example parser
3. **AGENT-TIMEOUTS.md** — Default/max timeouts, SLA per operation
4. **AGENT-QUICKSTART.md** — Step-by-step: multi-agent setup, claim workflow, error handling
5. **METRICS.md** — Prometheus metrics reference
6. **MIGRATIONS.md** — Schema changes, rollback procedures

---

**Owner:** TBD  
**Status:** Ready for prioritization  
**Last updated:** 2026-10-19
