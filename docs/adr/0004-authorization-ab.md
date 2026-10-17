# ADR-0004 — Scion/Culebra authorization A/B

- **Status:** accepted
- **Date:** 2026-10-17
- **Related:** `docs/W6-authorization-ab.md`, work order §10, ADR-0002 D4

## Context

Scion enforces its own policy inside every handler (`PolicyHandle`, path/ref rules). The V0 work
order requires the existing CulebraLuxe security model to be added **behind an A/B adapter**, not
replaced, with four modes: `SCION_ONLY`, `SHADOW_COMPARE`, `DUAL_ENFORCE`, `CULEBRA_ONLY`. In
shadow mode Culebra must not override Scion; in dual mode both must allow. The decision for each
request — subject, action, repository, path, both results, final result, timestamp — must be
persisted.

The CulebraLuxe model is already in the same Neon database: `auth_identity → app_user →
app_user_role → security_role → role_authority → authority`. It is user-centric. There are no
service-principal `app_user` rows, and the live identities are Google today, while Scion's human
login is GitHub. Those gaps are facts the A/B harness must expose, not paper over.

Wrapping `PolicyHandle` to call both systems was rejected: it is a vendored type threaded through
every handler, and a second policy implementation inside it would change Scion behavior. Calling
Culebra from inside each handler was also rejected: it duplicates the call site in every handler
and makes it easy to add a handler that forgets.

## Decision

- One `AuthorizationService` gate in the REST façade, after `authenticate` and before the
  handler. It evaluates Scion (the same `Policy` the handler will use) and, in every mode except
  `SCION_ONLY`, Culebra. The handlers keep their own Scion enforcement; the gate is an additional
  check, never a replacement.
- Scion is authoritative in `SCION_ONLY` (default) and in `SHADOW_COMPARE`; Culebra cannot
  override it. `DUAL_ENFORCE` requires `scion_allow && culebra_allow`. `CULEBRA_ONLY` is accepted
  by configuration but answers `501 unimplemented`: the vendored handlers still enforce Scion, so
  serving it would be a false claim.
- Culebra is evaluated from the existing tables through a small `deadpool-postgres` pool on the
  same database (TLS, Neon). The mapping for source-control is provisional:
  - `root` role, or `tech.access` authority → read, write and ref updates;
  - `portal.read` authority → reads only;
  - no Culebra identity (every `service:*` principal today, and any GitHub login without a
    matching `auth_identity`) → deny.
  This is a starting point for shadow evidence, not a final Culebra entitlement design.
- Decisions are inserted into `forgevcs_auth_decisions` (ForgeVCS-owned table, D6). The full DDL
  lives in `migrations/0001_forgevcs_auth_decisions.sql`; `AuthorizationService::from_config`
  also runs the idempotent DDL at startup so a deployment without a migration runner still gets
  the table. The runtime copy is a bootstrap, not a second schema.
- `DUAL_ENFORCE` fails closed when the Culebra evaluator or the audit insert is unavailable.
  `SHADOW_COMPARE` logs the failure and keeps Scion's decision, because the whole point of shadow
  is to observe a second system that may not be ready.
- The action is coarse: `read`, `write`, or `update_ref`, with the repository/path/ref in the row.
  The endpoint maps path-specific reads and writes; `push` evaluates write on `/` and the
  handler still applies exact per-path policy.

## Consequences

- `SCION_ONLY` is unchanged in behavior and adds no Culebra latency; the default service does not
  open a Culebra pool or create the table.
- `SHADOW_COMPARE` costs one Culebra query and one insert per business request. That is the price
  of evidence; it is not a production hot-path mode.
- Service agents are a deliberate mismatch today: Scion allows them; Culebra has no identity for
  them, so it denies. `DUAL_ENFORCE` therefore blocks them until Culebra identities/roles are
  created. That is why the initial production mode is `SCION_ONLY`, then shadow.
- The live Culebra `auth_identity` provider is `google`; Scion's human login is GitHub. A GitHub
  login will not match a Culebra identity until an adapter maps it. The harness records that as a
  mismatch rather than inventing a mapping.
- `CULEBRA_ONLY` cannot be served while handlers still enforce Scion. If Culebra-only becomes a
  requirement, the handler policy seam must change first; this ADR does not pretend otherwise.
- A request that Scion allows and Culebra denies is recorded with `mismatch = true`; the final
  column shows which system won. Query `forgevcs_auth_decisions` for the shadow report.
- The table is created at startup in shadow/dual modes. A future deployment should run
  `migrations/0001_forgevcs_auth_decisions.sql` as the canonical migration and may then remove
  the runtime DDL.

## Alternatives rejected

- **Wrap `PolicyHandle`** — a vendored type used inside every handler; changing its behavior is a
  rewrite and risks Scion semantics.
- **Put the Culebra call inside each handler** — the same call site repeated, and easy to miss on
  a new endpoint.
- **Return Culebra's decision in `CULEBRA_ONLY`** — the handlers would still enforce Scion, so the
  service would claim a mode it does not implement.
