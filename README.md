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
- Vendored **with no source modifications**; `upstream/.git` was excluded, nothing else was.
- Scion is **Apache-2.0**; its licence is retained at `upstream/LICENSE` and mirrored at
  `LICENSE`. Copyright remains with Scion's authors.
- Changes to vendored code, when they start, are listed here. As of W1 there are none:
  W2 adds `upstream/crates/scion-store-crdb/src/dialect.rs` behind a `postgres` feature,
  per ADR-0001 §7.1.

## Build and test

```sh
cargo test -p forgevcs-service          # the service
cargo build --release --locked          # what the container runs
upstream/scripts/check.sh               # the vendored upstream gate (T-core) — must exit 0
upstream/scripts/check-layers.sh        # module-boundary check
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
