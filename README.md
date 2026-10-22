# ForgeVCS

Source-control engine for CulebraLuxe, built on [Scion](https://github.com/CarterT27/scion)
— Carter's source-control engine, vendored read-only as `upstream/`.

**Status:** W0–W3 accepted (upstream baseline, service shell, Postgres port, object storage).
W4 parts 1–3 delivered (engine wiring, auth seam, business façade), W5 delivered
(WorkspaceService over Scion's working copy) and W6 delivered (Scion/Culebra A/B authorization);
residual gaps are recorded in `docs/W4-engine-wiring.md` §8.6, `docs/W5-workspace-service.md` §7
and `docs/W6-authorization-ab.md` §6. See `docs/` for the binding record;
`docs/adr/0001-infrastructure-stack.md` … `docs/adr/0004-authorization-ab.md` are the decisions
of authority.


## Hosted service (orientation — read first)

- **Hosted URL:** `https://culebraluxe-forgevcs.vercel.app` (separate Vercel project, not lane-deep).
- **Health:** `GET /health`, `GET /ready` (200 only when Postgres + object bucket answer),
  `GET /version` (service version + upstream Scion SHA).
- **Deploy:** `bash scripts/deploy-forgevcs-prod.sh` (must run from `main`; wraps the Vercel
  project deploy, prints the deployment URL and rollback instructions). Hosted smoke:
  `FORGEVCS_HOSTED_URL=https://culebraluxe-forgevcs.vercel.app bash scripts/w8-hosted-smoke.sh`
  (needs the external service-principal file; do not put secrets in the repo).
- **Human CLI target:** `forgevcs --base-url https://culebraluxe-forgevcs.vercel.app …`
  (token from `forgevcs auth mint`; token file 0600). Hosted human principals are configured by
  `FORGEVCS_HUMAN_PRINCIPALS`; the local dev harness is documented in
  `scripts/lane-scope-acceptance.sh`.
- **Environments:** hosted production (Vercel + Neon), local acceptance (`forgevcs-ci` Neon branch
  or a throwaway local PG/S3 harness). Never run production-mutating tests against the hosted URL.

## Layout

| Path | What it is |
| --- | --- |
| `crates/forgevcs-service` | The Axum service. `http.rs` serves `/health`, `/ready`, `/version`, the auth seam (`/auth/token`, `/auth/whoami`) and merges Scion's `/healthz`, `/readyz`, `/metrics` unmodified; `facade/` serves `/repos/{repo}/init`, `/push`, `/pull`, `/refs`, `/commits`, `/changesets/{id}`, `/file`, `/tree`, `/diff`, `/merge-base` and the `/workspaces…` routes; `workspace.rs` is the WorkspaceService over `scion-wc`; `authz.rs` is the Scion/Culebra A/B gate; `engine.rs` boots the Scion `ServerApp` on Neon (metadata + objects), its private Unix-socket gRPC surface, and publishes a readiness snapshot |
| `acl.toml` | The policy file the engine loads (deny-by-default; agents only — ADR-0002 D4) |
| `upstream/` | Vendored Scion at `e0ffadf9`. A **separate cargo workspace** (`exclude = ["upstream"]` at the root) with its own gate. Read-only: adapt at the edges, do not rewrite |
| `docs/` | `adr/0001-infrastructure-stack.md` … `adr/0004-authorization-ab.md`, `BASELINE.md` (W0), `W1-service-shell.md` … `W8-hosted-smoke.md`, `M1-readiness-and-multi-agent.md` |
| `migrations/` | ForgeVCS-owned tables; `0001_forgevcs_auth_decisions.sql` is the W6 A/B decision log |
| `vercel.json` | The container service and catch-all rewrite for the separate Vercel project |
| `Dockerfile.vercel` | Release build + slim runtime, policy at `/etc/forgevcs/acl.toml`, non-root |
| `Dockerfile.vercel` | The deploy artefact: release build, non-root uid 10001, listens on `$PORT` |

## Upstream provenance and licence

- Upstream: `CarterT27/scion` @ `e0ffadf9c755f1b99812df5beffcabdebcb38c19`, tagged
  `FORGESC-SCION-UPSTREAM-BASELINE` in the source repo.
- Vendored with `rsync -a --exclude .git --exclude target`, so only VCS metadata and build
  output were dropped.
- Scion is **Apache-2.0**; its licence is retained at `upstream/LICENSE` and mirrored at
  `LICENSE`. Copyright remains with Scion's authors.
- **Modifications to vendored code.** W1 had none. W2 (ADR-0001 §7.1) adds the PostgreSQL
  dialect, entirely behind the `postgres` feature — the default build emits exactly the
  upstream CockroachDB statements:
  1. `crates/scion-store-crdb/src/dialect.rs` (**new**) — `Dialect::{Cockroach, Postgres}`:
     migration rewrites, inline cast rewrites, retry budget, session options, rustls roots.
  2. `crates/scion-store-crdb/src/store.rs` — dialect-selected upserts and `UNNEST` casts,
     dialect session options, TLS in `connect_fresh_for_tests`, a `connect_raw` helper.
  3. `crates/scion-store-crdb/src/store/txn.rs` — `adapt_inline_sql` in the three
     statement-preparation helpers; the attempt budget moved to `Dialect::max_attempts`.
  4. `crates/scion-store-crdb/src/migrations.rs` — `migrations()`, the dialect-adapted set.
  5. `crates/scion-store-crdb/tests/neon_conformance.rs` (**new**) — the conformance suite
     run against Postgres/Neon.
  6. `Cargo.toml` (workspace) and the crate manifest — `postgres` feature, `webpki-roots`
     pinned to the version already in `Cargo.lock`.
  7. `crates/scion-store-crdb/src/store.rs` (W4) — `CrdbMetaStore::ping()`, a `SELECT 1`
     used only by ForgeVCS readiness (ADR-0002 D5). Reads; no behaviour changed.
  8. `crates/scion-server/src/lib.rs` (W4) — `mod interceptor;` became `pub mod interceptor;`,
     so the façade can call the same `authenticate` the gRPC stack runs (ADR-0002 D4).
     Visibility only; no behaviour changed.
  9. `crates/scion-policy/src/policy.rs` + `crates/scion-server/src/handlers/admin.rs` (M1) —
     two DevOps entitlements, `admin.repos` (create/delete repositories) and `admin.trunk`
     (advance the trunk ref): new `RawAdmin` fields, `AdminPerm` variants, grants and permission
     names. Additive: a policy without the keys parses and denies by default, so the M0 policy
     keeps its exact behaviour until it opts in.
 10. `crates/scion-history/src/rename.rs` (**new**) + `rebase.rs`/`merge.rs` (M1) — rename
     detection (exact content, plus line-set similarity for rename + edit) re-anchors edits on
     both the rebase/land path and the explicit `Merge(a, b)` path. Enabled only when
     `FORGEVCS_MERGE_ENGINE=exact-rename-v1`; with the variable unset (the default, including the
     vendored gate) the behaviour is unchanged, and the module tests cover the safe fallbacks.
 11. `crates/scion-server/src/handlers/write.rs` (task-87) — conflict payloads fail closed when a
     conflicting path is writable but unreadable, instead of returning a filtered partial report.
     Local policy patch; see `docs/TECH_DEBT.md` TD-12. It must be carried or upstreamed at the
     next vendored-pin bump.

  No SQL statement's logic changed; only its dialect spelling. Proof is the suite, not
  inspection: 20/20 cases of `meta_store_suite` pass against Neon.

## Engine

The service boots one Scion engine against Neon at startup (ADR-0002):

```sh
FORGEVCS_DATABASE_URL=postgres://…neon… \
FORGEVCS_BUCKET=forgevcs-objects \
AWS_REGION=aws-us-east-2 \
AWS_ENDPOINT_URL_S3=https://…neon… \
FORGEVCS_POLICY_PATH=./acl.toml \
FORGEVCS_SERVICE_PRINCIPALS="maestro=$(printf %s "$SECRET" | shasum -a 256 | cut -d' ' -f1)" \
  forgevcs-service
```

`/ready` is 200 only when the metadata store answers `SELECT 1` **and** the bucket answers a
`HEAD`; otherwise it is 503 with the reason. `acl.toml` is deny-by-default and grants only
the agent principals (`service:maestro`, `service:opencode`) — no human identity has access
until W5.

**Auth seam.** `POST /auth/token` takes a configured service principal and its secret and
returns the same signed token `AuthService.Login` would mint; `GET /auth/whoami` spends it.
Both call the vendored handlers in-process through `interceptor::authenticate` (ADR-0002 D4),
so a token minted here is also valid for every gRPC RPC — there is one token format, not two.

```sh
TOKEN=$(curl -s -X POST localhost:3000/auth/token -H 'content-type: application/json' \
  -d "{\"service\":\"maestro\",\"secret\":\"$SECRET\"}" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
curl -s localhost:3000/auth/whoami -H "authorization: Bearer $TOKEN"
# {"service":"forgevcs","status":"ok","principal":"service:maestro","groups":["agents"],...}
```

A wrong secret and a missing token are both 401 (`invalid service credentials`, `missing
bearer token`); with no engine reachable both endpoints are 503, never 500.

**Business façade.** Every endpoint authenticates first; the handler receives the verified
`CallerCtx` and keeps its own policy checks (ADR-0002 D4).

| Endpoint | What it does |
| --- | --- |
| `POST /repos/{repo}/init` | Boots the repo's engine; creates the Scion `repos` row and bootstraps root + `main`. Idempotent |
| `GET /repos/{repo}/refs` | All refs with target + version |
| `GET /repos/{repo}/commits?ref=main&limit=100` | Resolve a ref, then `Log` |
| `POST /repos/{repo}/pull` | Resolve + `Log`, plus optional object bytes by id |
| `POST /repos/{repo}/push` | `UploadObjects` → `CreateChangeset` → `Land` (default) or `UpdateRef` |
| `GET /repos/{repo}/changesets/{id}` | Decoded changeset: parents, root tree, delta |
| `GET /repos/{repo}/file?changeset=&path=` | File entry and base64 bytes at a changeset |
| `GET /repos/{repo}/tree?changeset=&path=` | One directory's entries |
| `GET /repos/{repo}/diff?a=&b=&paths=` | Changed paths, entries and text hunks |
| `GET /repos/{repo}/merge-base?a=&b=` | Best common ancestors |

Object ids are Scion text form (`b1…`, `c1…`); object data is base64. `push` takes
`{"objects":[…],"changeset":{…},"ref":"main","mode":"land"}` and keeps the no-ref-moved
guarantee: a failed upload or changeset never lands. A repo that was not booted in this
process answers 404 until `init`; reads never create one. Full shapes and findings are in
`docs/W4-engine-wiring.md` §8.

**Workspace façade.** A workspace is a disposable `scion-wc` materialization in a temp
directory; canonical history stays in Neon. The internal working copy talks to the engine's
private Unix-socket gRPC surface, and the façade checks the external caller's policy before
every path operation (ADR-0003).

| Endpoint | What it does |
| --- | --- |
| `POST /repos/{repo}/workspaces` | Materialize `{ref}`/`{changeset}` (default `main`), optional `sparse` |
| `GET /repos/{repo}/workspaces/{id}` | Status: modified/added/deleted/mode-changed/redacted-dirty |
| `POST /repos/{repo}/workspaces/{id}/materialize` | Move to another ref/changeset (`force` for dirty files) |
| `GET/PUT/DELETE /repos/{repo}/workspaces/{id}/file?path=` | Read/write/delete one file (base64 data) |
| `GET /repos/{repo}/workspaces/{id}/diff?context=3` | Working-copy diff with hunks |
| `POST /repos/{repo}/workspaces/{id}/submit` | Commit, then land (`mode:"land"`, default) or leave unlanded |
| `DELETE /repos/{repo}/workspaces/{id}` | Destroy the materialization |

Workspace state and its dirty-file overlay are persisted in Neon (`forgevcs_workspaces`,
`forgevcs_workspace_files`); the temp directory is a cache that any instance can restore. The
17-check `scripts/w5-acceptance.py` proves the work order's disposal property: destroy the
workspace, recreate it, and the submitted file is still there.

**Authorization A/B.** Every business request runs `AuthorizationService` after the token is
verified. `FORGEVCS_AUTH_MODE` selects `SCION_ONLY` (default), `SHADOW_COMPARE`, `DUAL_ENFORCE`
or `CULEBRA_ONLY` (accepted, not served). Shadow records the Culebra decision and keeps Scion's;
dual requires both. Culebra is read from the existing `auth_identity → app_user → … → authority`
tables on the same Neon database, and each evaluated request is appended to
`forgevcs_auth_decisions`. See `docs/W6-authorization-ab.md`.

```sh
FORGEVCS_AUTH_MODE=SHADOW_COMPARE scripts/w6-authz-smoke.sh
# shadow refs 200, dual refs 403, two persisted decision rows, invalid mode /ready 503
```

## Deployment

The service is its own Vercel project, `culebraluxe-forgevcs`:
**https://culebraluxe-forgevcs.vercel.app**. The release is compiled locally in Docker (`devops/Dockerfile.build`, linux/amd64) and Vercel
unpacks a tiny runtime image (`devops/Dockerfile.runtime`) with the binary and `acl.toml`; the
project is `framework=container` and listens on `PORT=3000` (non-root cannot bind 80).
`scripts/deploy-forgevcs-prod.sh` builds, stages, deploys, waits for `/ready`, checks the live
service and optionally runs W8. Production secrets include Neon, object storage, the service
principal and `FORGEVCS_SIGNING_KEY_PEM`; see
[`docs/W7-vercel-deployment.md`](docs/W7-vercel-deployment.md).

```sh
scripts/deploy-forgevcs-prod.sh --smoke   # local Docker compile + Vercel runtime deploy + W8
```

## Build and test

```sh
cargo test -p forgevcs-service          # the service (34 tests, no network)
cargo build --release --locked          # what the container runs

scripts/engine-smoke.sh                 # boot against the live Neon branch and probe it;
                                        # exits 0 only if the W4 (20 checks) and W5 (17 checks)
                                        # acceptance clients pass
                                        # needs .env.local (`neon env pull`)
python3 scripts/w4-acceptance.py --base http://127.0.0.1:3000 --token "$TOKEN" --repo forgevcs-smoke
python3 scripts/w5-acceptance.py --base http://127.0.0.1:3000 --token "$TOKEN" --repo forgevcs-workspaces
                                        # the same clients against any deployment (W8)
scripts/w6-authz-smoke.sh               # shadow 200 / dual 403 / persisted decisions
scripts/w8-hosted-smoke.sh              # W4/W5 against the Vercel deployment + Neon

scripts/gate.sh                         # the vendored upstream gate (T-core) — must exit 0
                                        # detached, one log per run:
                                        #   tail -3 ~/Library/Logs/forgevcs/latest.log
# Do not tail logs to find out where a run stands — ask, in one read and one exit code:
scripts/gate-status.sh                  # 0 pass, 1 fail, 2 still running, 3 never run
# The machine-wide ~/.cargo/config.toml points every checkout at one shared target dir, so
# concurrent lanes block on its lock ("Blocking waiting for file lock on build directory").
# Give the gate its own dir and it runs unattended instead; the log records which one.
CARGO_TARGET_DIR=/Users/Shared/dev/build/forgevcs scripts/gate.sh
upstream/scripts/check.sh               # the same gate, in the foreground, if you want to watch
upstream/scripts/check-layers.sh        # module-boundary check

# W2 acceptance: the full MetaStore conformance suite against Postgres/Neon.
# Unpooled URL only — the pooler breaks prepared statements and session SERIALIZABLE.
(cd upstream && SCION_TEST_CRDB_URL="$(grep '^DATABASE_URL_UNPOOLED=' ../.env.local | cut -d= -f2- | tr -d '"')" \
  cargo test -p scion-store-crdb --features postgres --test neon_conformance -- --nocapture)

# W3 acceptance: the blob contract against Neon Object Storage, driven through the
# service's own Config (path-style, HTTPS-only). Skips and passes when unconfigured.
set -a; . ./.env.local; set +a
FORGEVCS_BUCKET=forgevcs-objects FORGEVCS_KEY_PREFIX=forgevcs-ci \
  cargo test -p forgevcs-service --test neon_blob_conformance -- --nocapture
```

Gate on **T-core**. Do not gate on `check.sh --full`: it is red upstream at the baseline SHA
on one test at one hardcoded seed reachable only in full mode (`docs/BASELINE.md`).

## Configuration

All from the environment; no secrets in the repo. `FORGEVCS_*` names are preferred and fall
back to the Neon-injected equivalents:

| Variable | Falls back to | Notes |
| --- | --- | --- |
| `FORGEVCS_DATABASE_URL` | `DATABASE_URL` | Must be `postgres://` |
| `FORGEVCS_BUCKET` | `FORGEVCS_S3_BUCKET` | Bucket is **branch-scoped** on Neon |
| `FORGEVCS_AUTH_MODE` | — | `SCION_ONLY` (default), `SHADOW_COMPARE`, `DUAL_ENFORCE` or `CULEBRA_ONLY` |
| `FORGEVCS_SIGNING_KEY_PATH` | — | PKCS#8 PEM Ed25519 file; preferred on a host with files |
| `FORGEVCS_SIGNING_KEY_PEM` | — | The same key inline for a platform that injects secrets as values (Vercel) |
| `FORGEVCS_AWS_REGION` | `AWS_REGION` | Object Storage supports four AWS regions |
| `FORGEVCS_AWS_ENDPOINT_URL_S3` | `AWS_ENDPOINT_URL_S3` | Rejected if orphaned without a bucket |
| `PORT` | — | Default `3000`; Vercel injects it |
