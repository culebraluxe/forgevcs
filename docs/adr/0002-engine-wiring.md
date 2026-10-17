# ADR-0002 — Wiring the Scion engine into ForgeVCS

- **Status:** accepted
- **Date:** 2026-10-17
- **Supersedes:** the naming in the original work order (ForgeSC) — see D1
- **Related:** ADR-0001 (infrastructure stack), `docs/BASELINE.md`, `docs/W3-object-storage.md`

## Context

W0–W3 proved the pieces in isolation: the upstream suite is green on the vendored
commit, the metadata port passes the upstream conformance suite against Neon, and
the object store passes both conformance suites against a real bucket. None of it
is reachable from the running service: `forgevcs-service` serves three probes and
opens no database or bucket connection.

W4 ("SourceControlService") inherits a discovery from reading the vendored code:
`scion-server` already implements the entire source-control surface as six
`scion.v1` gRPC services, with policy and auth enforced inside its handlers. There is
no HTTP surface upstream. Building a second source-control engine would duplicate
Carter's engine rather than wrap it, which the work order forbids (§3, §2.1).

This ADR records the decisions that wiring requires, so the next four workstreams
do not re-litigate them.

## Decisions

### D1 — The product is ForgeVCS

The work order names the service ForgeSC (`FORGESC_*`, `forgesc-service`). That
predates the rename. ForgeVCS is final:

- binary and crate: `forgevcs-service`
- environment: `FORGEVCS_*`
- JSON `service` field: `"forgevcs"`
- tags: `w<N>-<slug>`

Rationale: the rename predates any deployment, so nothing external depends on
`FORGESC_*`; the cost of switching is a docs edit now versus a Vercel environment
migration later.

### D2 — Neon Postgres and Neon Object Storage are the only deployed backends

Deployments use exactly one metadata store and one object store: Neon.

The vendored Cockroach path stays in the tree because it is the reference
implementation the conformance suite is written against, and deleting it would fork
upstream for no operational gain. It is never routed: no `FORGEVCS_*` value selects
it, and no deployed configuration mentions it. D2 is about what runs, not about what
compiles.

### D3 — One engine per repository, not one engine with a repo column

`repo_id` is intrinsic to a `Storage` instance and is baked into object keys
(`<prefix>/r/<repo_id>/…`, SPEC 5.1). Multi-tenancy inside one store would mean
changing upstream's `MetaStore`/`Storage` traits, which D2's "preserve upstream"
argument forbids without a measurement to justify it.

So: `Engines` is a registry of `Engine` values keyed by repository name. An engine
owns one `ServerApp`, one metadata store, one object-store handle and one policy
handle. Engines are created on demand and cached for the process lifetime. This is
also what makes the work order's `create_repository`/`delete_repository` (gaps in
upstream, §W4) implementable at our layer without touching the storage trait.

### D4 — The auth seam is `interceptor::authenticate` plus in-process handlers

`scion_server::interceptor::authenticate` is public and turns a bearer token into a
`CallerCtx`. Handlers take a `tonic::Request` and read that `CallerCtx` from its
extensions. Therefore the HTTP layer can:

1. verify the token with the same `Verifier` the gRPC interceptor uses,
2. build a `tonic::Request`, insert the `CallerCtx`,
3. call the `handlers::*` function directly.

No loopback gRPC hop, no duplicated policy evaluation, no second authorization
implementation. Policy stays inside the handlers, where upstream put it.

**As vendored, `interceptor` was a private module** (`mod interceptor;`), so the seam D4
depends on was unreachable. W4 makes the module `pub` — a visibility change with no
behaviour change (README modification log, item 8) — rather than re-implementing bearer
verification in our layer, which would be a second auth implementation by definition.

The first two endpoints built on the seam are `POST /auth/token` (the service-credential
branch of `Login`, so the constant-time secret comparison and the token TTL remain
upstream's) and `GET /auth/whoami` (which proves a minted token is accepted by the verifier
every later façade call will use). Both were exercised live before any business endpoint
existed: a façade handler without a `CallerCtx` in hand should not be writable.

Part 3 adds the business operations on the same seam: `POST /repos/{repo}/init` (boot an
engine, which creates the Scion `repos` row and bootstraps root + `main`), `push`
(`UploadObjects` → `CreateChangeset` → `Land`/`UpdateRef`), `pull` (`ResolveRef` → `Log` →
`GetObjects`), `GET refs` (`MetaStore::list_refs`) and `GET commits` (`ResolveRef` → `Log`).
The only code each adds is JSON ↔ `scion.v1` mapping; policy, validation and land semantics
stay in the handlers. The one exception is the ref list, because upstream has no `ListRefs`
RPC: it calls the store after the same `read` on `/` check `GetRefLog` performs.

This is also the single insertion point for W6's A/B harness: `SCION_ONLY` is today's
behaviour, and `SHADOW_COMPARE`/`DUAL_ENFORCE` wrap decision *evaluation* at this one
call site rather than at every endpoint.

### D5 — `/ready` probes reachability; `/health` does not

- `/health` is liveness: the process is serving. It never touches a dependency.
- `/ready` is readiness: configuration is coherent, at least one engine exists, the
  metadata store answers `SELECT 1`, and the bucket answers a `HEAD`.
- Probes run on a background task every 5 s with a 2 s per-dependency timeout; the
  synchronous `Readiness::check` reads the cached snapshot. A readiness endpoint that
  awaits the database would turn a slow Neon into a slow probe and a restart loop.
- Boot failures do not crash the process. The reason is logged once and surfaced in
  `/ready` until it clears, which matches how a container platform is expected to
  behave and keeps `/health` honest.

### D6 — ForgeVCS tables never mix with Scion core tables

Scion's fourteen tables (`repos`, `objects`, `object_paths`, `object_uploads`,
`changesets`, `changeset_children`, `refs`, `ref_log`, `landed`, `derived`,
`derive_leases`, `redactions`, `counters`, `audit_log`) belong to the engine. Forge
concepts — story/execution mapping, delegation decisions, executor claims, repo
registry — go in `forgevcs_*` tables added by our own migrations. The work order's
"do not prematurely add Storyboard/Forge fields to Scion's core tables" (§6) is
upheld; `workspace` is deliberately absent until W5 models it.

### D7 — Deviations from the work order, recorded

| Work order | Here | Why |
|---|---|---|
| §6 `forgesc-store-neon` crate | `postgres` feature inside vendored `scion-store-crdb` | The conformance suite must run unmodified against both dialects; a separate crate would copy the store |
| §1/§11 `FORGESC_*`, `forgesc-service` | `FORGEVCS_*`, `forgevcs-service` | D1 |
| §5 `/ready` "200" | 200 only when dependencies answer | D5; a config-only check was the W1 stopgap |
| §12 TLS on the app port | TLS terminates at the platform; the gRPC listener is not exposed in M0 | The engine is called in-process (D4), so upstream's TLS material is unused |

## Consequences

- W4 is a façade: JSON ↔ `scion.v1` messages, plus the four gaps upstream does not have
  (`create_repository`, `delete_repository`, `rollback`, a decided meaning for `integrate`).
  Part 3 implements the five business operations; `create_repository` is
  `POST /repos/{repo}/init` (a Scion `repos` row plus the bootstrap), while deletion,
  `rollback` and `integrate` remain open.
- Repositories are created on demand and live in `Engines` for the process lifetime. Their
  Scion `repos` rows and history are durable, but a restart enumerates nothing: a non-default
  repo answers 404 until `init` runs again. D6's `forgevcs_repos` registry is the right place
  to fix that and to hold Forge-only metadata (owner, visibility, workspace); it is not added
  here because duplicating the `repos` source of truth before W5 needs it buys nothing.
- There is one token verifier per process (the default engine). An ephemeral keyring is
  generated per engine, so using the addressed repo's verifier would reject a token minted by
  `/auth/token` on a repo initialized later. With `FORGEVCS_SIGNING_KEY_PATH` set, all engines
  load the same key and the distinction disappears.
- `acl.toml` needs an exact `"main"` rule as well as `"refs/heads/*"`: upstream's bootstrap
  uses the literal ref `main`, which the pattern does not match. The root bootstrap bypasses
  policy, so its absence only appears on the first push.
- The gRPC tower stack does not wrap these handlers. Per-principal rate limiting, load
  shedding, request-id propagation, RPC log events and epoch trailers are applied around the
  generated gRPC services, not inside the handlers, so every REST endpoint — not only
  `/auth/token` — skips them. REST responses carry no `scion-policy-epoch` /
  `scion-redaction-epoch` / `scion-tier` trailers, and `push` must be rate-limited at the
  platform until the limiter is reused at the seam.
- Auth is not optional plumbing later; every endpoint added in W4 goes through
  `authenticate` from its first commit, so `SCION_ONLY` is real rather than nominal.
- Until signing keys are configured, the engine generates an ephemeral key and logs a
  warning: tokens do not survive a restart. Production must set
  `FORGEVCS_SIGNING_KEY_PATH`.
- `acl.toml` ships at the repository root and is loaded by every engine. It is deny-by-
  default and grants only `service:maestro` and `service:opencode` (the `[groups] agents`
  group): read/write on every path, `update` on `main` and `refs/heads/*`, no force, no
  `admin.*`. A repository with no policy file cannot boot — that is the intended failure, not
  a gap.
- The engine needs TLS even though the app port does not: Neon rejects plaintext, so
  `engine.rs` passes `dialect::tls_client_config()` (webpki roots) into `CrdbConfig`.
- `/auth/token` is unauthenticated by necessity — it *is* `Login`. It inherits upstream's
  constant-time comparison and rejection log, but not the gRPC stack's per-principal rate
  limiter (`ratelimit.rs` is wired inside `ServerApp`, which the façade does not route
  through). Until that limiter is reused at this call site, the endpoint must sit behind
  platform-level rate limiting; noted rather than silently accepted. The business endpoints
  added in part 3 inherit the same gap (see above).
