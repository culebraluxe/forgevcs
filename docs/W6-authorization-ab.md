# W6 — Authorization A/B: Scion and Culebra

**Status:** complete 2026-10-17. The façade evaluates Scion and Culebra for the same request,
persists the decision, and serves `SCION_ONLY` by default. `SHADOW_COMPARE` allows what Scion
allows and records the mismatch; `DUAL_ENFORCE` denies when either side denies. The live
acceptance is `scripts/w6-authz-smoke.sh`.

## 1. What this part solves

The work order requires both security systems to coexist while the CulebraLuxe model is proven:

```text
SCION_ONLY      Scion decides, Culebra not consulted (default)
SHADOW_COMPARE  Scion decides; Culebra evaluated and recorded, never overrides
DUAL_ENFORCE    ALLOW + ALLOW = ALLOW; anything else = DENY
CULEBRA_ONLY    accepted by config, not served (501) until Scion can be bypassed
```

The forbidden shape is `Scion OR Culebra`. The service never implements it: shadow uses Scion,
dual uses AND.

## 2. Where it runs

`AuthorizationService` is created once in `main` and held by `AppState`. Every business endpoint
calls it after `authenticate` and before the vendored handler:

```text
request → authenticate (CallerCtx)
        → AuthorizationService (Scion policy + Culebra decision)
        → vendored handler (Scion policy again, unchanged)
```

The handler's own Scion enforcement is untouched. In `SCION_ONLY` the gate returns Scion's
decision and opens no database connection for Culebra.

The Culebra evaluator reads the existing tables on the same Neon database:

```text
auth_identity → app_user → app_user_role → security_role → role_authority → authority
```

The provisional source-control mapping is recorded in
[ADR-0004](adr/0004-authorization-ab.md): `root` or `tech.access` allows read/write/ref;
`portal.read` allows read; no identity denies. Service principals have no Culebra `app_user`, so
they are the first, deliberate shadow mismatch.

## 3. Persisted decision

`forgevcs_auth_decisions` (ForgeVCS-owned, ADR-0002 D6) carries one row per evaluated request:

| Column | Meaning |
| --- | --- |
| `subject` | `service:maestro`, `github:login`, … |
| `action` | `read`, `write`, `update_ref` |
| `repository` / `path` / `ref_name` | the request coordinates |
| `scion_allow` / `culebra_allow` | each system's decision (`culebra_allow` nullable) |
| `final_allow` | what the façade did |
| `mismatch` | the two decisions differ |
| `mode` | `SHADOW_COMPARE` or `DUAL_ENFORCE` |
| `reason` | why the final decision was taken |
| `at` | timestamp |

The migration is `migrations/0001_forgevcs_auth_decisions.sql`; `AuthorizationService` also runs
the idempotent DDL at startup in shadow/dual modes. A shadow example from the live run:

```text
service:maestro | read | forgevcs-ab | / | scion=t | culebra=f | final=t | mismatch=t | SHADOW_COMPARE
```

## 4. Acceptance

```sh
cargo fmt --all --check
cargo clippy -p forgevcs-service --all-targets -- -D warnings
cargo test -p forgevcs-service --lib --bins
scripts/w6-authz-smoke.sh
scripts/gate.sh
scripts/engine-smoke.sh
```

`w6-authz-smoke.sh` starts two short-lived processes against the live Neon branch:

```
shadow-refs      HTTP 200
dual-refs        HTTP 403
shadow-decision  2 row(s)     # scion=t, culebra=f, final=t, mismatch=t
dual-decision    2 row(s)     # scion=t, culebra=f, final=f
invalid-mode     HTTP 503     # /ready rejects an unknown FORGEVCS_AUTH_MODE
W6_AUTHZ=PASS
```

`engine-smoke.sh` remains the `SCION_ONLY` regression: it must still pass unchanged (W4 20 +
W5 17 checks). The default mode does not touch Culebra.

## 5. Findings

1. **The live Culebra identities are Google, Scion's humans are GitHub.** `auth_identity.provider`
   is `google` for all current rows. A GitHub login does not match any Culebra identity yet. The
   harness records the mismatch instead of inventing a mapping; a provider adapter is future work.
2. **Service principals have no Culebra app_user.** `service:maestro` is a Scion service
   principal, not a Culebra user. Shadow records Scion ALLOW / Culebra DENY; `DUAL_ENFORCE`
   correctly denies it. The agents need Culebra service identities before dual mode is usable.
3. **The source-control authority mapping is provisional.** Culebra has no source-control
   authority codes; `tech.access` and `portal.read` are the closest existing capabilities. Treat
   the current mapping as a shadow experiment, not the final Culebra policy.
4. **`CULEBRA_ONLY` is not served.** The vendored handlers still enforce Scion, so a true
   Culebra-only request path does not exist. The mode returns 501 rather than claiming success.
5. **The runtime DDL is a bootstrap.** A deployment without a migration runner still gets the
   table; the SQL file is the canonical definition and should replace the bootstrap when a
   migration runner exists.
6. **Shadow mode needs a working Culebra pool.** If the pool is unavailable, the request keeps
   Scion's decision and the failed insert is logged; dual fails closed. This is deliberate, but
   the shadow report is empty during that outage.

## 6. Remaining gaps

- **Culebra identity mapping** for GitHub humans and service agents (provider adapter and/or
  service app_users).
- **Authority mapping review**: decide which Culebra authorities should mean source-control read,
  write and admin; do not leave `tech.access` as the permanent answer.
- **Culebra-only enforcement** requires a handler policy seam that can bypass Scion.
- **Shadow report tooling**: the table is queryable, but no endpoint/report summarizes mismatches
  for the operator yet.
- **Decision retention**: no purge/retention policy on `forgevcs_auth_decisions`; the W6 table is
  append-only and will grow with every shadow request.
