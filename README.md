# ForgeVCS

Source-control engine for CulebraLuxe, built on [Scion](https://github.com/CarterT27/scion)
— Carter's source-control engine, vendored read-only as `upstream/`.

**Status:** W0–W3 accepted (upstream baseline, service shell, Postgres port, object storage).
W4 parts 1–3 delivered (engine wiring, auth seam, business façade); the residual gaps —
repository deletion, restart recovery for non-default repos, human login, and the gRPC layers
the façade skips — are recorded in `docs/W4-engine-wiring.md` §8.6. See `docs/` for the binding
record; `docs/adr/0001-infrastructure-stack.md` and `docs/adr/0002-engine-wiring.md` are the
decisions of authority.

## Layout

| Path | What it is |
| --- | --- |
| `crates/forgevcs-service` | The Axum service. `http.rs` serves `/health`, `/ready`, `/version`, the auth seam (`/auth/token`, `/auth/whoami`) and merges Scion's `/healthz`, `/readyz`, `/metrics` unmodified; `facade.rs` serves `/repos/{repo}/init`, `/push`, `/pull`, `/refs`, `/commits` over the in-process handlers; `engine.rs` boots the Scion `ServerApp` on Neon (metadata + objects) and publishes a readiness snapshot |
| `acl.toml` | The policy file the engine loads (deny-by-default; agents only — ADR-0002 D4) |
| `upstream/` | Vendored Scion at `e0ffadf9`. A **separate cargo workspace** (`exclude = ["upstream"]` at the root) with its own gate. Read-only: adapt at the edges, do not rewrite |
| `docs/` | `adr/0001-infrastructure-stack.md`, `adr/0002-engine-wiring.md`, `BASELINE.md` (W0), `W1-service-shell.md`, `W2-postgres-port.md`, `W3-object-storage.md`, `W4-engine-wiring.md` |
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

## Build and test

```sh
cargo test -p forgevcs-service          # the service (30 tests, no network)
cargo build --release --locked          # what the container runs

scripts/engine-smoke.sh                 # boot against the live Neon branch and probe it;
                                        # exits 0 only if the 20-check W4 client passes:
                                        # init → A → B → parent → history → file/tree/diff/merge-base
                                        # needs .env.local (`neon env pull`)
python3 scripts/w4-acceptance.py --base http://127.0.0.1:3000 --token "$TOKEN" --repo forgevcs-smoke
                                        # the same client against any deployment (W8)

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
| `FORGEVCS_AWS_REGION` | `AWS_REGION` | Object Storage supports four AWS regions |
| `FORGEVCS_AWS_ENDPOINT_URL_S3` | `AWS_ENDPOINT_URL_S3` | Rejected if orphaned without a bucket |
| `PORT` | — | Default `3000`; Vercel injects it |
