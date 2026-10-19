# R4 — per-workspace write lease

Status: **implemented** (2026-10-19). Source: Culebraluxe lane-deep
`docs/agent/REVIEW-ACTION-PLAN.md` WO-R4 ("Workspace writes last-writer-wins — no distributed
lock"). Scope: single-instance dev/prod, but concurrent agents can address one workspace.

## Problem

A workspace is a disposable working copy: each request restores the base state and the dirty-file
overlay from Neon (`forgevcs_workspaces`, `forgevcs_workspace_files`) into a fresh `TempDir`,
mutates it, and writes the overlay back. The process-local `Mutex` in `WorkspaceService::entry`
does not span requests or processes, so two agents writing the same workspace raced and the last
persisted overlay silently won. `docs/W5-workspace-service.md` §7 listed this as a known gap.

## Design

A row in `forgevcs_workspace_leases`, keyed by workspace id, is the single writer's claim:

| column | meaning |
|---|---|
| `workspace_id` | primary key, `REFERENCES forgevcs_workspaces(id) ON DELETE CASCADE` |
| `repo` | repository of the workspace |
| `owner_id` | authenticated principal (`service:agent-a`, `github:culebraluxe`) |
| `lease_id` | fencing token; rotates on takeover, stable across renewals |
| `acquired_at` | when this ownership started |
| `heartbeat_at` | last renewal |
| `expires_at` | stale after this instant (now + TTL, default 300 s) |

Every decision is **one transaction**: `INSERT … ON CONFLICT (workspace_id) DO NOTHING`
(serialises the first writer), `SELECT … FOR UPDATE` (serialises refresh/takeover/conflict), then
one of three branches:

- **same owner** → refresh `heartbeat_at`/`expires_at`, keep the token (idempotent re-acquire);
- **foreign owner, expired** → takeover: rotate `owner_id`/`lease_id`, and write a `takeover` row
  to `forgevcs_workspace_lease_events` naming the prior owner, prior token and prior expiry, in
  the same transaction;
- **foreign owner, live** → no write; the caller gets a typed conflict naming the current owner.

Postgres is the arbiter, so two processes cannot both win. A lease table was chosen over
`pg_advisory_lock` because an advisory lock belongs to a pooled session and cannot outlive the
request that took it; the 409 must report a durable owner and expiry.

### Acquire / release points (`crates/forgevcs-service/src/workspace.rs`)

| path | lease behaviour |
|---|---|
| `write_file` | acquire before restoring the working copy; renew immediately before the overlay write |
| `delete_file` | same as `write_file` |
| `materialize` (goto) | acquire before restore; renew before persisting state/overlay |
| `submit` | acquire before restore; renew before commit and again before landing the ref; release on every exit path |
| `destroy` | acquire; delete the workspace; audit a `released` event (the release row is removed by cascade, the audit survives) |
| `create`, `status`, `diff`, `read_file` | no lease (read-only; `create` makes a new workspace id, so there is nothing to collide on) |

The final renew on each write path is the fencing check: if another owner took the lease over
while this request was working, the persist/land is abandoned with the conflict instead of
overwriting the new owner's state.

Releasing on `submit`/`destroy` lets the next writer continue immediately. An abandoned lease
(agent died) becomes available after the TTL; the next writer takes it over with an audit row.
There is deliberately no public force-release endpoint in R4.

## Conflict surface

`WorkspaceService` returns `Aborted` with the owner in gRPC metadata; the façade decodes it into
the typed error below (other `Aborted` statuses keep the generic conflict mapping). `repo`'s
`workspace_result` calls `ApiError::lease_conflict_from_status`, and the `api_error` tests pin
the envelope.

```
HTTP/1.1 409 Conflict
{"service":"forgevcs","status":"error","code":"workspace_lease_conflict",
 "message":"workspace <id> is leased by service:agent-a until unix 1700000300",
 "details":{"kind":"workspace_lease","workspace_id":"<id>","repo":"pilot",
            "owner":"service:agent-a","lease_id":"<token>","expires_at_unix":1700000300}}
```

The caller can back off, wait for `expires_at_unix`, or escalate to the owner. It is never
treated as a successful write.

## Configuration

- `FORGEVCS_WORKSPACE_LEASE_TTL_SECS` — lease lifetime in seconds. Default **300** (5 min).
  `0` is refused by `Config::validate` (it would make every lease instantly stale). Invalid
  non-numeric values fall back to the default, like the other integer settings.

## Schema

`migrations/0004_forgevcs_workspace_leases.sql` is the readable record. `lease::ensure_schema`
runs the same idempotent DDL when `WorkspaceService::connect` boots (the repository has no
ordered migration runner yet — WO-R6), so a deployment without the runner still gets the tables.
Keep the two definitions in step; the runtime copy is the bootstrap, not a second schema.

The audit table (`forgevcs_workspace_lease_events`: `acquired`, `released`, `takeover`) has no
foreign key to the workspace on purpose: `destroy` deletes the workspace (cascading the lease
row), and the takeover/release evidence must outlive it.

## Tests and evidence

Unit tests (`cargo test -p forgevcs-service --lib lease`):

- `concurrent_writers_one_wins` — 8 writers released by a barrier; exactly one grant, every loser
  gets the winner's owner/token/expiry.
- `an_expired_lease_is_taken_over_with_audit` — live lease refused at TTL−1 s, takeover at TTL,
  rotated token, prior owner in the grant and in the audit.
- `release_allows_the_next_writer` — release → next owner acquires with no takeover.
- same-owner re-acquire is a heartbeat; heartbeat extends expiry and blocks takeover; foreign
  heartbeat reports the owner; foreign/idempotent release; destroy records a release without a
  live row; conflict → `Aborted` status metadata; missing row → not-found; TTL floor.
- `postgres_store_enforces_the_same_contract` — the same free/conflict/takeover/release/next
  sequence against a **real Postgres**, gated on `FORGEVCS_TEST_DATABASE_URL` (skips otherwise,
  so `cargo test` needs no database). Verified against a throwaway local PostgreSQL 16 cluster
  (`initdb` in a temp dir, test database, cluster removed afterwards; no shared or production
  branch was written).

`api_error` tests pin the 409 envelope (code, owner, token, expiry) and that a non-lease
`Aborted` is not misread. `config` tests pin the 5-minute default, the override and the `0`
refusal. The service suite is green: 90 lib tests + 3 integration tests.

## Limitations / follow-ups

- The lease owner is the **principal**, not an individual agent process. Two processes sharing one
  service principal can still interleave; a per-instance owner id would need a caller-supplied
  token bound to the principal (R2-style authorization), not part of R4.
- Reads are not leased. A reader can observe an intermediate overlay while the owner is mid-write;
  reads never overwrite, so this is a stale-read window, not data loss.
- A single operation that runs longer than the TTL could in principle lose the lease before its
  final renew; the renew-then-persist order turns that into a 409, not an overwrite. Very large
  materializations on a sub-5-minute TTL should raise the TTL.
- Lease/audit rows are not garbage-collected (an audit row per acquisition/takeover/release).
  Retention is a separate housekeeping item (cf. auth-decision retention, R6).
- No ordered migration runner yet: the runtime DDL bootstrap is the applied schema (WO-R6).

## Files

- `crates/forgevcs-service/src/lease.rs` — store, service, audit, memory store + tests
- `crates/forgevcs-service/src/workspace.rs` — acquire/renew/release wiring
- `crates/forgevcs-service/src/api_error.rs` — `WorkspaceLeaseConflict` + status decoding
- `crates/forgevcs-service/src/facade/responses.rs` — `workspace_result` maps the typed 409
- `crates/forgevcs-service/src/config.rs` — TTL configuration
- `migrations/0004_forgevcs_workspace_leases.sql` — schema record
