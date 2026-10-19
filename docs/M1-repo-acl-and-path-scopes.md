# M1 — repository-scoped ACL (FVCS-01) and path-prefix push scopes (FVCS-02)

**Status:** implemented 2026-10-18 (WO FVCS-01 + FVCS-02, production packet P0-1/P0-2).
**Scope:** `crates/forgevcs-service/src/{repo_acl.rs,authz.rs,facade.rs,config.rs}`, `acl.toml`
(comments only), this doc. No vendored `upstream/` change; no migration file; no database change
was executed to write this.

## The two gaps this closes

`docs/PRODUCTION-PACKET.md` P0-1/P0-2 and `docs/M1-agent-pilot.md` "What this does not prove":

1. **`acl.toml` is path/ref-scoped, not repository-scoped.** The vendored Scion policy cannot see a
   repository name, so `service:agent-a` could read and write `service:agent-b`'s repository.
2. **`push` required write on `/`.** Scion's `create_changeset` checks write on every changeset
   delta path, but the façade's coarse `write on "/"` gate ran first, so an agent with write on
   `lane/a/**` could never push: every agent needed write everywhere.

## FVCS-01 — repository identity layer

Every business request now evaluates **two mandatory layers**, combined with AND:

```text
bearer token -> principal -> Scion path/ref policy
                           AND repository ACL (owner / configured readers+writers / admin bypass)
                           -> handler
```

`crates/forgevcs-service/src/repo_acl.rs` owns the second layer:

- **Owner assigned on `init`.** The first successful `POST /repos/{repo}/init` claims the
  repository for the authenticating principal. The claim is first-writer-wins
  (`INSERT … ON CONFLICT DO NOTHING` in `forgevcs_repo_acl`), so an idempotent init or a concurrent
  instance cannot reassign an owner. Only a principal with `admin.repos` (the DevOps seat) may
  re-initialize a repository it does not own.
- **Default deny.** An unknown repository, a repository with no owner row (legacy repo created
  before this feature), or a principal that is neither owner nor granted is refused with
  `403 permission_denied`. The reason is explicit ("owned by another principal;
  cross-repository access is denied") without disclosing repository contents.
- **Operator rules.** `FORGEVCS_REPO_ACLS` (JSON) pre-assigns rules for the multi-agent pilot,
  where the DevOps seat creates the repositories:

  ```json
  {
    "pilot-a": {"owner":"service:agent-a","readers":[],"writers":["service:agent-a"]},
    "pilot-b": {"owner":"service:agent-b","readers":[],"writers":["service:agent-b"]}
  }
  ```

  `owner` has read/write/update-ref; `readers` read only; `writers` read/write/update-ref. The
  value is validated by `Config::validate` (JSON shape, `service:`/`github:` principals, repository
  names) so a typo fails `/ready` instead of silently disabling isolation. Configured rules are
  authoritative: `init` cannot overwrite them.
- **Admin bypass is repository identity only.** `admin.repos` lets DevOps administer any
  repository, but it does **not** grant path writes: the path/ref policy still decides those.
- **Storage.** Service-owned table `forgevcs_repo_acl` (`repository`, `owner`, `readers`,
  `writers`, `created_at`) created idempotently at startup, the same precedent as W6's
  `forgevcs_auth_decisions`. No database is configured (unit tests, storage-less process): the
  registry is in-memory and does not survive restart; that is documented as test/pilot-only.
- **Fail closed.** A repository ACL store that cannot answer surfaces `503 unavailable`; it is
  never converted to an allow.
- **Deletion releases ownership.** `DELETE /repos/{repo}` removes the `forgevcs_repo_acl` row in
  the same transaction as the repository rows, so a recreated repository starts unowned instead
  of inheriting the deleted repository's principal. Operator-configured rules are config and are
  not removed.

`AuthorizationService::authorize` is the choke point for every route whose handler relies on the
façade's path/ref pre-flight (pull, refs, commits, changeset, file, tree, diff, merge-base,
workspaces, push). `AuthorizationService::authorize_repository` applies the **same repository
layer alone** to operations the vendored handler authorizes itself: `merge` and `land`
(write), `rebase` (read), `merge-land` (write) and `ref create` (update-ref). Those handlers
previously inherited only upstream's path/ref check, so without this call a principal could act
in another agent's repository; `authz::tests::repository_identity_applies_to_upstream_checked_operations`
covers the refusal. `AuthorizationService::authorize_provisioning` is init-only and skips the
repository layer because the owner does not exist until after the engine boots;
`claim_repo_owner` runs immediately after.

## FVCS-02 — path-prefix push scopes

`POST /repos/{repo}/push` now authorizes **each changeset delta path** instead of write on `/`:

- `facade::push` extracts every `changeset.delta[].path` before upload/create. The upload of
  content-addressed objects precedes their naming, so the delta is the only place a push names
  the paths it writes.
- `AuthorizationService::authorize_push` evaluates `write` on each path (path policy AND
  repository identity). One refused path refuses the whole push; the refusal names the policy
  decision, not the file contents.
- An **empty delta** has no path to scope and keeps the coarse write-on-`/` gate (fail safe).
- The coarse gate is **kept** for repository/ref provisioning: `init` (write on `/` +
  `admin.repos`), `ref_create`, `POST /refs`, trunk advance and the ref rules are unchanged.
- Upstream `create_changeset` still applies its own per-path `PolicyWriteAuthz` check after the
  façade; the façade check is the pre-flight that makes path scopes usable, the engine check is
  defense in depth. Nothing about the engine's path/ref semantics changed.

Result: an agent with `write = ["service:agent-a"]` on prefix `lane/a` (and no root write) can
push `lane/a/**` and is refused `lane/b/**` or `src/**`; ref scopes still work.

**Rollout note.** The checked-in `acl.toml` still grants `write` on `/` to `group:agents` — the
pre-FVCS-02 default every agent depended on. The push gate is now path-fine, but the *policy* only
restricts paths once an operator narrows that root grant to the lane prefixes an agent may write.
That is a policy decision per deployment (path grants differ per lane), so this change documents
the migration and tests the mechanism rather than silently taking write away from every agent and
breaking a live pilot.

## `acl.toml`

The vendored policy parser is strict (`deny_unknown_fields`), so repository rules cannot live in
`acl.toml` — a `[repos]` table would make the file unloadable. The policy file keeps its path/ref
rules and now documents where repository identity is evaluated and how to pre-assign rules
(comments only; no behavior change).

## Tests / acceptance evidence

```sh
cargo test -q -p forgevcs-service --lib --bins
upstream/scripts/check.sh
```

New tests (all offline; no database, no model, no network):

- `repo_acl`: configured-rule parsing/typo rejection/name rules; first-init claim,
  idempotent re-claim, second-principal `OwnedByOther`; owner read/write/update-ref allowed and
  cross-repository denied; reader/writer grants; default-deny for ownerless/unknown repositories;
  configured rule cannot be reassigned by `init`.
- `authz`: cross-repository read, write and update-ref denied while same-repository operations
  are allowed; `admin.repos` bypasses repository identity but not path policy; `authorize_push`
  allows `lane/a/**`, refuses `lane/b/**`, refuses a mixed delta, keeps the coarse gate for an
  empty delta, and refuses a cross-repository push.
- `config`: valid `FORGEVCS_REPO_ACLS` accepted; malformed value rejected by `validate` with the
  variable named.
- `facade`: every changeset delta path is extracted for authorization.

## Residual work (not in this work order)

- **Rules admin API** (P1): grant/revoke readers/writers per repository without an env edit.
  `forgevcs_repo_acl` already has the columns; the issue is the operator surface.
- **Legacy repositories** created before FVCS-01 have no owner row and are denied to agents;
  the DevOps seat can read them (admin bypass) and `FORGEVCS_REPO_ACLS` can assign owners.
  A one-off backfill (owner = `service:maestro`) is an operator decision, not this change.
- **Workspace path scopes**: workspace endpoints are repository-scoped now, but their internal
  working-copy checks still require root read/write for some operations; path-scoped workspace
  submissions remain a follow-up.
- **Unreferenced object uploads**: a principal with write on any path can still upload
  content-addressed blobs in `objects[]`; only naming them in a delta is path-checked. That is a
  storage-abuse surface covered by P0-3 quotas / GC, not by path authorization.
- **DB-backed ACL path**: the in-memory registry is what the unit suite exercises (no database is
  touched by this work order). The Postgres implementation uses the same idempotent-DDL precedent
  as `forgevcs_auth_decisions`; its SQL is compile-checked but a live round-trip is part of the
  next pilot/soak (P0-4), not claimed here.
