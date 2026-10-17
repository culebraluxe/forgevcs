# W1 — ForgeVCS service shell (acceptance record)

**Status: PASS** — 2026-10-17. Binary starts, all probe routes answer, upstream
gate re-verified against the vendored tree.

## Decision: own deploy crate now, not a piggyback

Carter's ruling on 2026-10-17 was "create a second deploy crate … eventually I need to
split it out to own — maybe it's just easier to do now" with Vercel Pro available. So
ForgeVCS is its own cargo workspace, its own binary, its own `Dockerfile.vercel` and its
own Vercel project from the start. Nothing is grafted onto another deploy.

## Layout

```
forgevcs/
├── Cargo.toml              # ForgeVCS workspace; exclude = ["upstream"]
├── Cargo.lock
├── rust-toolchain.toml     # 1.98.0, upstream's pin
├── Dockerfile.vercel       # the only Vercel-specific artefact
├── crates/
│   └── forgevcs-service/   # the deployable unit (bin)
├── upstream/               # vendored Scion, its OWN workspace, gate intact
└── docs/
```

`upstream/` keeps `scripts/check.sh`, `SPEC.md`, `docs/contracts/`, `schemas/`,
`rustfmt.toml`, `clippy.toml` and `scripts/check-layers.sh`, so the W0 gate means exactly
what it meant before vendoring.

**One trap, recorded because it bit us:** cargo automatically claims any path dependency
that lives inside the workspace directory as a *member* of that workspace, which then
resolves vendored Scion's `version.workspace = true` against **our** root:

```
error: error inheriting `repository` from workspace root manifest's `workspace.package.repository`
```

The fix is `exclude = ["upstream"]`, which tells cargo those crates belong to the
`upstream/` workspace. Without it, every inherited field (`version`, `edition`,
`rust-version`, `license`, `publish`, `repository`, `[lints]`) would silently come from
ForgeVCS instead of Scion.

## What the service does (W1 scope only)

- Binds `0.0.0.0:$PORT` (default 3000), graceful shutdown on Ctrl-C **and SIGTERM**.
- `/health`, `/ready`, `/version` — the work order's spelling, returning
  `{service, status, version}`; `/version` also reports the pinned upstream commit.
- `/metrics`, `/healthz`, `/readyz` — Scion's `scion_obs::health_router`, mounted
  unmodified. Aliases, never renames: upstream's own tests keep passing.
- Structured JSON logging via `scion_obs::init_logging`.
- Config from the environment only (`FORGEVCS_DATABASE_URL` → `DATABASE_URL`,
  `FORGEVCS_BUCKET`, `FORGEVCS_AWS_REGION` → `AWS_REGION`,
  `FORGEVCS_AWS_ENDPOINT_URL_S3` → `AWS_ENDPOINT_URL_S3`). No secrets in the repo.

Readiness is deliberately **config coherence only** for M0 — it answers 503 with a reason
when the configuration cannot be served. Reachability probes arrive with the store (W2)
and storage (W3); a readiness check that cannot fail is worse than none.

## Evidence

```
$ PORT=43117 forgevcs-service
/health    {"service":"forgevcs","status":"ok","version":"0.1.0"}            [200]
/ready     {"service":"forgevcs","status":"ready","version":"0.1.0"}         [200]
/version   {"…","upstream":"e0ffadf9c755f1b99812df5beffcabdebcb38c19"}       [200]
/healthz   ok                                                                [200]
/readyz    ready                                                             [200]
/metrics   scion_* series                                                    [200]
```

`cargo test -p forgevcs-service` → **10 passed, 0 failed** (5 config parsing, 5 routes,
including "ready is 503 when the database URL is not postgres"). `cargo fmt --check` and
`cargo clippy -D warnings` clean.

## Neon state

- Branch **`forgevcs-ci`** (`br-muddy-wind-ax2iknwx`) on project `CulebraluxeData`,
  region `aws-us-east-2`, state `ready`.
- Verified reachable: `PostgreSQL 18.6` via `psql`, `neondb_owner` has DDL rights.
- A branch, not a new project, because the account reports **`Projects Limit 0`**. A
  branch is copy-on-write isolated, so the conformance suite's `CREATE`/`DROP TABLE`
  cannot touch production. If a dedicated project becomes possible, migrate by
  re-pointing `FORGEVCS_DATABASE_URL` — nothing in the code assumes either shape.

## Not done yet (honest list)

- **The image has never been built** — no Docker daemon running locally. `Dockerfile.vercel`
  is written and plausible, not proven.
- **No Vercel project created/linked yet**; that needs `vercel link` in this directory and
  the `PORT`-injection path exercised for real.
- gRPC (`scion-server`) is still not exposed; REST-only, as ADR-0001 §6 requires.
