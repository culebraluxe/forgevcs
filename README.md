# ForgeVCS

Source-control engine for CulebraLuxe, built on [Scion](https://github.com/CarterT27/scion)
— Carter's source-control engine, vendored read-only as `upstream/`.

**Status:** W0 (baseline) and W1 (service shell) accepted. W2 (Postgres port of the store)
is next. See `docs/` for the binding record; `docs/adr/0001-infrastructure-stack.md` is the
decision of authority.

## Layout

| Path | What it is |
| --- | --- |
| `crates/forgevcs-service` | The Axum service: `/health`, `/ready`, `/version`, plus Scion's own `/healthz`, `/readyz`, `/metrics` merged unmodified |
| `upstream/` | Vendored Scion at `e0ffadf9`. A **separate cargo workspace** (`exclude = ["upstream"]` at the root) with its own gate. Read-only: adapt at the edges, do not rewrite |
| `docs/` | ADR-0001, `BASELINE.md` (W0), `W1-service-shell.md`, `W3-object-storage.md` |
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

  No SQL statement's logic changed; only its dialect spelling. Proof is the suite, not
  inspection: 20/20 cases of `meta_store_suite` pass against Neon.

## Build and test

```sh
cargo test -p forgevcs-service          # the service
cargo build --release --locked          # what the container runs

scripts/gate.sh                         # the vendored upstream gate (T-core) — must exit 0
                                        # detached, one log per run:
                                        #   tail -3 ~/Library/Logs/forgevcs/latest.log
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
  cargo test -p forgevcs-service --test neon_blob_conformance -- --nocapture)
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
