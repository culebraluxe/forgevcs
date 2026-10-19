# ForgeVCS — instructions for agents

**The project is ForgeVCS** (source-control engine: Carter's Scion, vendored). Repo root is
`/Users/Shared/dev/src/forgevcs`.


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

Layout: ForgeVCS is its own cargo workspace (`crates/forgevcs-*`) and its own Vercel
deploy (`Dockerfile.vercel`). `upstream/` is vendored Scion and is a **separate** cargo
workspace — the root manifest sets `exclude = ["upstream"]` so cargo resolves Scion's
inherited fields against Scion, not against us. Do not merge the two workspaces.

**Version control.** The root is a plain git repo; first commit `c37c343`, tagged
`w1-service-shell`. Carter runs **jj in front of git** on his other repos
(`Culebraluxe-web` is a jj+git colocated repo), so keep history linear and commits
described rather than terse. Identity is `culebradev <culebraluxe@gmail.com>`.
`upstream/` is committed as **plain files, not a submodule**, so the baseline SHA is
inspectable with no network. `.env.local` and `.neon/` are gitignored and must stay
that way — they hold live Neon credentials (`neon env pull` writes them, and rotates
them on every run).

**Workstream state.** W0 baseline PASS; W1 service shell PASS (tag `w1-service-shell`);
W2 Postgres port PASS (2026-10-17); W3 object storage PASS (2026-10-17, tag
`w3-object-storage`); **W4 part 1 — engine wiring — PASS (2026-10-17)**: the service boots
the Scion engine on Neon and `/ready` is 200 only with `SELECT 1` + bucket `HEAD` answering;
`cargo clippy … -D warnings` 0, `cargo test -p forgevcs-service --lib --bins` 21 passed, live
smoke on `forgevcs-ci` green. **W4 part 2 — auth seam — PASS (2026-10-17)**: `POST
/auth/token` mints real Scion tokens for configured service principals and `GET /auth/whoami`
spends them through `interceptor::authenticate`, so every later façade call starts from a
verified `CallerCtx`. **W4 part 3 — business façade — PASS (2026-10-17)**: `POST
/repos/{repo}/init|push|pull` and `GET /repos/{repo}/refs|commits|changesets/{id}|file|tree|diff|merge-base`
map JSON onto the vendored handlers in-process; `cargo test -p forgevcs-service --lib --bins` 30
passed, clippy 0, the upstream gate is unchanged at 1343, and `scripts/engine-smoke.sh` runs the
20-check `scripts/w4-acceptance.py` client against Neon (A → B, parent, history, both file
versions, tree, diff, merge-base) and exits 0. Repository deletion, human login, and the
gRPC layers the façade skips are the open W4 gaps
(`docs/W4-engine-wiring.md` §8.6). See `docs/W4-engine-wiring.md`. **W5 — WorkspaceService —
PASS (2026-10-17)**: `WorkspaceService` wraps the vendored `scion-wc` over a private
Unix-socket gRPC surface (ADR-0003); `POST/GET/PUT/DELETE /repos/{repo}/workspaces…` cover
create/materialize/status/read/write/delete/diff/submit/destroy. Workspace state and its
dirty-file overlay are persisted in Neon so any instance can restore an ID; the temp
directory is a cache, and canonical source history stays in Neon changesets/objects. `scripts/w5-acceptance.py` passes 17 checks
(create → write → diff → submit → destroy → recreate; history survives), the service has 30
tests, clippy is 0, and the upstream gate is 1343. See `docs/W5-workspace-service.md`. **W6 —
authorization A/B — PASS (2026-10-17)**: every business request evaluates Scion and, in shadow or
dual mode, the CulebraLuxe auth tables; `forgevcs_auth_decisions` records both results and the
final decision. `SCION_ONLY` is the default; `SHADOW_COMPARE` allows what Scion allows and records
mismatches; `DUAL_ENFORCE` requires both. `scripts/w6-authz-smoke.sh` proves 200/403 plus persisted
rows. See `docs/W6-authorization-ab.md` and ADR-0004. **W7 — Vercel deployment — PASS
(2026-10-17)**: separate project `culebraluxe-forgevcs`, local Docker compile
(`devops/Dockerfile.build`) + tiny runtime image (`devops/Dockerfile.runtime`), project
`framework=container`, policy baked at `/etc/forgevcs/acl.toml`, signing key through
`FORGEVCS_SIGNING_KEY_PEM`; production is
https://culebraluxe-forgevcs.vercel.app. **W8 — hosted smoke — PASS (2026-10-17), M0**: the W4
(20 checks) and W5 (17 checks) clients pass against the hosted deployment and Neon, with no Git
in the source-control operations; `scripts/w8-hosted-smoke.sh` is the receipt. See
`docs/W7-vercel-deployment.md` and `docs/W8-hosted-smoke.md`. The Postgres port does not change
the default build: it lives behind `--features postgres`, and the object-storage additions are
additive.

**Find out where things stand with one command, not by reading logs.**
`scripts/gate-status.sh` prints one JSON line and exits 0 pass / 1 fail / 2 running / 3 never
run. A session that lost its context starts there.

Read these before changing anything, in this order:

1. [`docs/adr/0001-infrastructure-stack.md`](docs/adr/0001-infrastructure-stack.md) — the
   **binding** infrastructure decision: runtime, Vercel deployment shape, Neon Postgres,
   Neon Object Storage, the database-access ruling, module boundaries, the forbidden list,
   and the platforms verified on 2026-10-16 (§9).
2. [`docs/BASELINE.md`](docs/BASELINE.md) — the W0 acceptance record: upstream SHA, pinned
   toolchain, the gate command, measured T-core/T-full/T-external results, and the seams
   later workstreams exploit.
3. [`docs/W1-service-shell.md`](docs/W1-service-shell.md) — the service that exists today:
   routes, env config, and what is deliberately unfinished.
4. [`docs/W2-postgres-port.md`](docs/W2-postgres-port.md) — the meta store ported to
   Postgres/Neon: the dialect seam, the acceptance command, and the four findings that only
   running the suite found.
5. [`docs/W3-object-storage.md`](docs/W3-object-storage.md) — the object-storage work
   order mapped clause-by-clause onto the vendored source (§1 says what is **already
   implemented** and must not be rebuilt), and §8 what W3 actually added: the extended blob
   suite, `Config::s3_config()`, and the Neon acceptance run.
6. [`docs/adr/0002-engine-wiring.md`](docs/adr/0002-engine-wiring.md) +
   [`docs/W4-engine-wiring.md`](docs/W4-engine-wiring.md) — how the engine is wired into the
   service (D3 engine registry, D4 in-process handlers, D5 readiness, D6 table split) and the
   live acceptance run, including the three findings that cost time (TLS, `Arc` coercion,
   the required `/` policy rule).
7. [`docs/adr/0003-workspace-service.md`](docs/adr/0003-workspace-service.md) +
   [`docs/W5-workspace-service.md`](docs/W5-workspace-service.md) — the WorkspaceService
   decision (internal Unix-socket gRPC, disposable temp-directory workspaces, external policy
   checks) and the 17-check live acceptance.
8. [`docs/adr/0004-authorization-ab.md`](docs/adr/0004-authorization-ab.md) +
   [`docs/W6-authorization-ab.md`](docs/W6-authorization-ab.md) — the Scion/Culebra A/B gate,
   its modes and provisional authority mapping, the persisted decision table, and the live
   shadow/dual acceptance.
9. [`docs/W7-vercel-deployment.md`](docs/W7-vercel-deployment.md) — the separate Vercel project,
   its container service, environment classes, production URL and deployment findings.
10. [`docs/W8-hosted-smoke.md`](docs/W8-hosted-smoke.md) — the hosted W4/W5 acceptance, the M0
    checklist, and what remains after M0.
11. [`docs/M1-readiness-and-multi-agent.md`](docs/M1-readiness-and-multi-agent.md) — the
    post-M0 readiness verdict, the prime-usage blockers by category, and the recommended
    multi-agent pilot topology and checks. Read this before starting M1 or an agent pilot.

## Hard rules

- **Do not substitute infrastructure.** Railway, AWS/GCP, Kubernetes, Redis, Kafka, NATS,
  Elasticsearch, extra worker services, and any additional database are out unless a
  measured requirement is written up as a new ADR and approved. If a step seems to need
  one, stop and write the ADR — do not quietly deviate.
- **WIP = 1.** One workstream in progress at a time. `W2` does not start until `W1`'s
  acceptance passes. No parallel "just getting started" branches.
- **`upstream/` is vendored Scion.** Adapt it at the edges (new crates, new adapters,
  aliases, a dialect module); do not rewrite its behaviour. Its `scripts/check.sh` must
  stay green.
- **Port, do not rewrite, the CockroachDB store** — ADR-0001 §7.1. The SERIALIZABLE
  transaction shape and the bounded `40001` retry loop already exist; re-deriving them is
  the expensive mistake.
- **Do not rebuild storage.** BLAKE3 hashing, zstd compression, content-addressed keys,
  idempotent puts and verify-on-read already exist in `scion-store`; see
  `docs/W3-object-storage.md` §1 before writing a line of `put`/`get`.
- **Canonical state lives in Neon only.** Local disk and Vercel Drives are cache/workspace.
  If a code path would make a filesystem authoritative, it is wrong.
- **Config comes from the environment.** No embedded secrets, no committed credentials.
  Neon Object Storage injects `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
  `AWS_ENDPOINT_URL_S3`, `AWS_REGION`; the service reads its own `FORGEVCS_*` variables and
  falls back to those.
- **Stay portable.** A boring `cargo build` → Docker → listen on `$PORT` binary. The only
  Vercel-specific artefact is the filename `Dockerfile.vercel`. No Vercel SDK calls in
  business logic; Daemons and Drives are later-phase, never V0 dependencies.

## Gate before claiming a workstream is done

```sh
upstream/scripts/check.sh             # vendored upstream gate — must exit 0
```

(Run it as `upstream/scripts/check.sh` from the repo root. The script re-roots itself with
`cd "$(dirname "$0")/.."`, so it does not exist at `scripts/check.sh` in this repo — only
inside `upstream/`.)

Gate on **T-core**. Do **not** gate on `check.sh --full`: it is red upstream at the
baseline SHA on one test at one hardcoded seed (`docs/BASELINE.md`, "T-full result"), so
T-full's rule is "no *new* failures versus that known set of one".

**Never wait on a log.** `scripts/gate.sh` runs detached and writes one log per run;
`./scripts/gate-status.sh` answers "pass / fail / running / never run" from that log in one
read, with an exit code you can trust in a script. That is the whole protocol: start the run,
do something else, ask once.

…plus the relevant Neon conformance run once a Neon backend exists — both are wired and
recorded: `meta_store_suite` via `cargo test -p scion-store-crdb --features postgres
--test neon_conformance`, and `blob_store_suite` / `blob_store_suite_extended` via
`cargo test -p forgevcs-service --test neon_blob_conformance` (skips and passes when the
object-storage variables are unset, so it is safe in a plain `cargo test`).

