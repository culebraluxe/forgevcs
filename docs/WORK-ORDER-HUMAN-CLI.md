# Work order — human CLI path (2026-10-19)

**Why:** ForgeVCS today has only service-principal tokens (`/auth/token`); no browser login, no
human principal, no `git`-like CLI, no Git wire protocol. The principal is a human and needs to run
the same operations agents do. Identity for this work: user `culebraluxe`, email
`culebraluxe@gmail.com`.

**Scope v1 (small vertical, not 180h):**
1. Human principal: add `culebraluxe` as a first-class principal (id/email/display), with an explicit
   token-mint path for a human (bearer token stored locally, 0600; no password in code). Audit-log
   the mint and every decision (existing `forgevcs_auth_decisions`).
2. `forgevcs` CLI (thin wrapper over the REST façade) with the commands a human needs:
   `auth` (mint/status), `init`, `refs`, `log`, `pull`, `push` (upload changeset), `merge`,
   `rebase`, `merge-land`, `workspace create/status/destroy`, `health`.
   Local config under `~/.config/forgevcs/`; target base URL configurable; no deploy/publish.
3. Acceptance: as `culebraluxe`, create a repo, push a changeset, read refs/log, merge, pull it
   back — all from the CLI, no browser. Tests for principal authorization and each command's happy
   path + one refusal.
4. Explicit non-goals: Git-compatible `git clone/push` (separate, large), browser UI, production
   enablement without principal sign-off.

**Estimate:** 1–2 engineer-days for the horizontal wrapper + principal, not weeks. The 180–220h
multi-agent estimate is not accepted; it includes speculative items unrelated to this path.

---

## v1 implementation notes (2026-10-19)

**Status:** implemented. Human principal + token-mint endpoint + `forgevcs` CLI + tests. No
Git wire protocol, no browser, no deploy path, no service-principal auth change.

### Server: human principal and token mint

- `FORGEVCS_HUMAN_PRINCIPALS` configures humans (JSON; only the SHA-256 digest of the secret is
  stored — never the secret, never a password in the repo):

  ```json
  {"culebraluxe":{"email":"culebraluxe@gmail.com","display":"Culebraluxe","secret_sha256":"<64 lowercase hex>"}}
  ```

  Generate the entry without putting the secret in argv:
  `printf '%s' "$SECRET" | scripts/human-principal-hash.sh --login culebraluxe --email culebraluxe@gmail.com --display Culebraluxe`
- `POST /auth/human-token {"login":"culebraluxe","secret":"…"}` verifies the digest in constant
  time (upstream's `scion_auth::ct_eq`; unknown logins compare against a dummy digest), then mints
  the normal engine bearer token for the **same** `github:culebraluxe` principal a browser login
  would issue. The secret and the token are never logged.
- Audit: the mint writes a `forgevcs_auth_decisions` row (`action = token_mint`), and human
  authorization decisions are written there too (in `SCION_ONLY`, only humans are audited so
  agent traffic keeps its existing zero-write behavior). The audit store is optional: a
  storage-less process logs a warning and still mints.
- Policy: `github:culebraluxe` is an ordinary member of `agents`/`devops` in the shipped
  `acl.toml`, so the human can init a repository, push/land, merge and read.
- Code: `crates/forgevcs-service/src/{human.rs,config.rs,authz.rs,http.rs}`.

### CLI: `forgevcs`

Build: `cargo build --release -p forgevcs-cli` → `target/release/forgevcs`.

Config under `~/.config/forgevcs/` (or `$XDG_CONFIG_HOME/forgevcs`, `$FORGEVCS_CONFIG_DIR`):

- `config.toml` (optional): `base_url = "https://…"`, `token_file = "token"`.
- `token` — bearer token, mode **0600** (tightened on read if it drifted).
- `session.json` — non-secret principal/email/display/expiry, mode 0600.

Environment: `FORGEVCS_BASE_URL`, `FORGEVCS_CONFIG_DIR`, `FORGEVCS_TOKEN_FILE` (and
`FORGEVCS_TOKEN` for a short-lived shell/CI token), `FORGEVCS_HUMAN_SECRET` (mint without stdin).
Flags `--base-url`, `--config-dir`, `--token-file` override everything.

```sh
forgevcs --base-url http://127.0.0.1:3000 auth mint --login culebraluxe
forgevcs init human-repo
forgevcs refs human-repo
forgevcs ref create human-repo --name lane/a --target <changeset-id> --reason "provision lane"
forgevcs log human-repo --ref main --limit 20
forgevcs push human-repo --changeset changeset.json --ref main
forgevcs merge human-repo --a <id> --b <id> --change-id <32 hex> --message "merge lane"
forgevcs pull human-repo --ref main --objects <object-id>
forgevcs merge-land human-repo --head <id> --ref main
forgevcs workspace create human-repo --ref main
forgevcs workspace status human-repo <workspace-id>
forgevcs workspace destroy human-repo <workspace-id>
forgevcs health
forgevcs auth status
```

`push` takes the façade `PushBody` JSON (`objects`, `changeset`, `ref`; `--ref`/`--mode`
override). A human still assembles that content-addressed body; a directory-snapshot builder is
future work (see non-goals). A new lane ref must be provisioned with `ref create` first: `push
--mode update` to a ref that does not exist is a façade `404 not_found` (there is no implicit
ref creation). `auth status` reports `token_present`/`token_source`, so `FORGEVCS_TOKEN` is
visible as `"env"` and a stored token as `"file"`.

### Tests

- `cargo test -q -p forgevcs-service --lib --bins` — human config/validation, constant-time
  verification, human authorization (allow + refusal), `/auth/human-token` (401/400/503 paths).
- `cargo test -q -p forgevcs-cli` — argument/config/token unit tests, plus an in-process HTTP
  mock exercising **every command** and the full acceptance flow
  mint → init → push → refs/log → merge → pull with the `github:culebraluxe` bearer token
  (and a 403 refusal kept as the façade's `permission_denied`).
- Gate: `upstream/scripts/check.sh`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --all -- --check`.

### Limitations (v1)

- The acceptance flow was run against an in-process façade mock, not a live Neon deployment:
  no production/DB mutation was permitted in this work order. The service-side handler tests
  prove the 401/400/503 paths; the live mint row is exercised only where a database is configured.
- `push` requires a hand-written `PushBody` JSON; no directory snapshot, no Git
  clone/fetch/push wire protocol (explicit non-goal).
- The terminal stdin path for `auth mint` is not echo-hidden (use `--secret-file` or
  `FORGEVCS_HUMAN_SECRET` for automation; a hidden prompt is a follow-up).
- No per-human rate limiting beyond the existing service limits (TD-4), and the mint endpoint is
  not held back by the deferred quota work.

### QA interim fixes (2026-10-19)

- **Dead CLI modules removed.** `crates/forgevcs-cli/src/{config,error,http}.rs` were an earlier
  parallel draft, unlinked from `lib.rs`; the shipped CLI is `args/client/commands/settings/token`
  only (their useful parts are in those modules; the crate was reformatted and unused `uuid`/
  `tower` dependencies dropped).
- **FVCS-01 gap fixed:** `merge`, `rebase`, `land`, `merge-land` and ref provisioning call the
  vendored handler directly, so they previously inherited the upstream path/ref check but not the
  service-side repository-identity layer. The façade now calls
  `AuthorizationService::authorize_repository` for each (read for rebase, write for
  merge/land/merge-land, update-ref for `ref create`), which is the same repository ACL and
  `admin.repos` bypass used by every other route. Test:
  `authz::tests::repository_identity_applies_to_upstream_checked_operations` proves cross-repository
  read/write/update-ref are refused for all three actions. The human acceptance flow is unaffected
  because the human is a repository owner/DevOps seat.
- **New-ref `push --mode update`:** fixed by adding the cheap `forgevcs ref create` wrapper (no
  implicit ref creation stays: the façade intentionally requires the provisioning seat and the
  FVCS-01 repository layer).
