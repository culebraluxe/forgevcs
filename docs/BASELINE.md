# ForgeVCS W0 — Upstream baseline (Scion)

**Status: T-core PASS, T-full FAIL(1), T-external BLOCKED (2026-10-16).** T-core is green;
T-full is deterministically red on one upstream test at one hardcoded seed (details under
[T-full result](#t-full-result)); T-external is blocked on this machine's Docker daemon.

This file is the W0 acceptance record. It is written *after* the measurements, and every
number here was read out of `/tmp/scion-baseline.log` from the run described in
[Reproduce](#reproduce) — not estimated.

## Provenance

| item | value |
| --- | --- |
| upstream | `https://github.com/CarterT27/scion` (public, Rust) |
| SHA | `e0ffadf9c755f1b99812df5beffcabdebcb38c19` |
| upstream commit date | 2026-10-16 13:50:20 -0500 — *same day as this baseline; the repo is live* |
| commits / branches / tags upstream | 207, `main` only, no tags |
| local clone | `/Users/Shared/dev/src/scion-upstream` (own `.git`, 4.1 MB) |
| baseline tag | `FORGESC-SCION-UPSTREAM-BASELINE` → commit `4b22a7b` (annotated tag on `e0ffadf9`) |
| build target dir | `/Users/Shared/dev/build/scion` (20 GB) — outside the clone, so the clone stays pristine |
| clone modified? | **no** — `git status --porcelain` empty before and after the run |

### Toolchain (pinned by upstream, not by us)

| item | value |
| --- | --- |
| `rust-toolchain.toml` | `channel = "1.98.0"`, components `rustfmt` + `clippy`, profile `minimal` |
| workspace `rust-version` | `1.98`, `edition = "2024"`, `resolver = "3"` |
| `rustc` used | `1.98.0 (88d9e12ae 2026-08-18)` |
| protoc | `libprotoc 36.2` (Homebrew `protobuf`; **required** — `scion-proto` will not build without it) |

## The gate

Upstream's own definition of done is `scripts/check.sh` (from `docs/PLAN.md` §0), and
that — not a suite we invent — is what W0 must pass:

```
cargo fmt --all -- --check
cargo build --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
scripts/check-layers.sh
```

## Result (T-core)

| metric | value |
| --- | --- |
| `scripts/check.sh` exit | **0** — `check.sh: all checks passed` |
| wall clock | **19m 56s** (cold build; load average ~11 from other work on this machine) |
| test result lines | 195 = 172 test binaries + 23 doc-test suites |
| tests **passed** | **1337** |
| tests **failed** | **0** |
| tests **ignored** | 50, over 35 binaries (the spec-sized `full_*` set — see T-full) |
| clippy | 0 warnings, 0 errors across 401 `Checking` units |
| fmt | clean (no diff) |
| layer check | passed |

**Known failures: none (T-core).** T-full has one upstream generator-guard failure — see
[T-full result](#t-full-result).

W0 acceptance — *"100% of tests that pass upstream must continue passing locally"* — is
satisfied for T-core: upstream's gate is green on this machine at the pinned SHA.

## Tiers

| tier | command | status |
| --- | --- | --- |
| **T-core** | `scripts/check.sh` | **PASS** (above) |
| **T-full** (nightly CI, allows 360 min) | `scripts/check.sh --full` → adds `SCION_FULL=1 cargo test --workspace --release -- --include-ignored` | **FAIL — 1445 passed / 1 failed / 50 ignored**, 16m 54s, `FULL_EXIT=101`. The single failure is upstream test fragility, not a Scion defect: see [T-full result](#t-full-result) |
| **T-external** (nightly CI) | `docker compose -f docker/compose.test.yml up -d --wait` then `scripts/test-external.sh --all` | **BLOCKED on this machine**: Docker CLI present, daemon not running (`~/.docker/run/docker.sock` absent). Compose pins `cockroachdb/cockroach:v24.3.5` + `bitnamilegacy/minio:2025.7.23` |

T-external is the tier that matters most to W2/W3, because it is the same machinery the
work order's acceptance asks for against Neon — see the next section.

## T-full result

`/tmp/scion-full.log`, `FULL_EXIT=101`, 16m 54s. Totals across every `test result:` line:
**1445 passed, 1 failed, 50 ignored**.

The failure is one test, and it is *deterministic* — not a flake, and not reachable from
T-core:

| item | value |
| --- | --- |
| test | `scion-cli --test cli_redact :: all_versions_matches_the_reference_model` |
| panic site | `crates/scion-cli/tests/cli_redact.rs:81` |
| message | `seed 23575: d1/f11.txt has too few versions to be a test` |
| why only in T-full | `cli_redact.rs:136-143` sweeps **3** seeds normally but **20** when `SCION_FULL` is set; seed `23575` is `0x5C10 + 7`, i.e. the 8th seed — unreachable below 8 |
| determinism | `scion-testkit`'s corpus generator uses `BTreeSet` and `ChaCha8Rng::seed_from_u64`, so the same seed always produces the same history. Re-running the exact test in isolation without `SCION_FULL` (8/8 passed) only ever exercised seeds `23568-23570` |
| defect class | **test precondition, not product behaviour.** `assert!(versions.len() >= 3, "seed {seed}: … has too few versions to be a test")` — a guard that panics when the generated history happens to be too thin, instead of skipping that seed |
| product risk | **none demonstrated.** The failing assertion runs *before* `redact` is exercised; the sibling `full_all_versions_matches_the_reference_model` (25 seeds × 150 commits, `0xF011`) **passed** |

**Verdict:** at `e0ffadf9`, upstream's T-core gate is green (1337/0) and its T-full tier is
red by exactly one self-referential generator guard. W0's acceptance clause — *"100% of
tests that pass upstream must continue passing locally"* — is therefore met for every test
that passes upstream, and the one failure is an upstream defect we should report rather than
patch inside the vendor tree (a one-line `continue` on thin seeds fixes it upstream).

**Consequence for W1+ acceptance:** do **not** gate ForgeVCS workstreams on `check.sh --full`
being green, because it is not green upstream at the baseline SHA. Gate on T-core, and treat
T-full as "no *new* failures versus this known set of one".


## Seams found while establishing the baseline

These were read from the source during W0. Each one changes the *size* of a later
workstream, so they are recorded here rather than discovered mid-W2.

1. **Object storage (W3) is configuration, not a new implementation.**
   `scion-store-s3` is built on the `object_store` crate (S3-compatible), and its
   acceptance tests are already env-gated. Neon Object Storage is S3-compatible, so W3's
   put/get/exists/verify, duplicate, large, missing, corrupt and parallel matrix is the
   existing suite pointed at a Neon endpoint:

   ```sh
   SCION_TEST_S3_ENDPOINT=… SCION_TEST_S3_BUCKET=… \
   SCION_TEST_S3_ACCESS_KEY=… SCION_TEST_S3_SECRET_KEY=… \
     cargo test -p scion-store-s3 --test s3_external
   ```

2. **Metadata (W2) is Postgres-wire portability, not a new store engine.**
   `scion-store-crdb` depends on `tokio-postgres` + `deadpool-postgres` +
   `tokio-postgres-rustls` — the standard Postgres wire client. Neon speaks that protocol.
   The acceptance suite is likewise env-gated:

   ```sh
   SCION_TEST_CRDB_URL='postgres://…' cargo test -p scion-store-crdb --test crdb_external
   ```

   **But three real divergences must be decided, not papered over** — this is exactly the
   "no behavioral divergence without an explicit architecture decision" clause:

   - `crates/scion-store-crdb/src/store.rs:49` — `AS OF SYSTEM TIME follower_read_timestamp()`
     for follower reads. Does not exist in Postgres/Neon.
   - `crates/scion-store-crdb/src/store.rs:80` — `UPSERT`. CockroachDB keyword; Postgres
     needs `INSERT … ON CONFLICT DO UPDATE`.
   - **Isolation**: CockroachDB is SERIALIZABLE by default; Neon Postgres defaults to
     READ COMMITTED. The work order requires **CAS update of `main`** and **idempotent
     retries**. **Measured 2026-10-16 — this is already solved upstream**: every method is
     one `BEGIN ISOLATION LEVEL SERIALIZABLE` transaction (`src/store/txn.rs:161`) with a
     bounded `40001`/`40003` retry loop (`store.rs:101`, `txn.rs:74`). W2 must *preserve*
     that, not invent it — see ADR-0001 §7.1.

3. **The conformance suite is a library we can reuse as-is.**
   `crates/scion-store/src/conformance.rs` exposes `meta_store_suite(factory)` (line 1496)
   and `blob_store_suite(factory)` (line 1685). Call sites: `crdb_external.rs:213` and
   `s3_external.rs:91`. Factory-based, so a Neon backend plugs in without touching the
   suite — this is what makes "same suite, both backends" cheap rather than a rewrite.

4. **The server shell (W1) already exists in axum.**
   `scion-obs` ships `health_router(metrics, readiness) -> axum::Router` with `/metrics`,
   `/healthz`, `/readyz`, behind a `Readiness` trait that **probes object storage and
   policy** on every `/readyz` request (`scion-server/src/app.rs:697-800`). `axum =
   "=0.8.9"` is already a pinned workspace dependency. So W1 is not "write a server"; it is

   - add `/health`, `/ready`, `/version` (the work order's spelling) as thin aliases in the
     ForgeVCS layer, keeping upstream's `z`-suffixed routes untouched so the upstream diff
     stays small, and
   - return the work order's JSON shape: `{service, status, version}`.

   Recommendation: alias, do not rename. Renaming upstream routes buys nothing and breaks
   upstream's own tests.

5. **What upstream gives us for free**: 23 crates, `SPEC.md`, `docs/contracts/`,
   `schemas/`, `proto/`, `rustfmt.toml`, `clippy.toml`, and `scripts/check-layers.sh` —
   architectural layering is *enforced*, which is why W1–W5 must respect layering rather
   than bypass it.

## Reproduce

```sh
# toolchain and protoc
rustup toolchain install 1.98.0     # pinned by rust-toolchain.toml
brew install protobuf               # scion-proto needs protoc

cd /Users/Shared/dev/src/scion-upstream
git rev-parse HEAD                  # must be e0ffadf9c755f1b99812df5beffcabdebcb38c19
CARGO_TARGET_DIR=/Users/Shared/dev/build/scion scripts/check.sh
```

`/tmp/scion-check.sh` is the heartbeat wrapper used for the recorded run (a beat every
20 s, so a long compile cannot be mistaken for a hang); it is deliberately outside the
clone.

## Open decisions (carried into W1+)

**Settled** by [ADR-0001](adr/0001-infrastructure-stack.md): the stack, the deployment
shape (including the 15 Oct 2026 container/Daemons/Drives releases), the database-access
ruling (**port** the CockroachDB store behind a dialect switch rather than rewrite it in
SQLx — §7.1), and the decision record required for the three CockroachDB → Neon divergences
listed under seam 2 above.

Still open:

1. **Where the ForgeVCS repo lives** — a fork under the org, or a fresh repo whose first
   commit is upstream `e0ffadf9` vendored as `upstream/scion-*`. W1 needs this before it
   can create the workspace shape. Docs and code live in
   `/Users/Shared/dev/src/forgevcs` (confirmed home; `git init` done 2026-10-17, no commit
   or remote yet).
2. **Which Neon project/branch W2 tests against. RESOLVED.** Never the CulebraLuxe
   production database; the conformance suite creates and drops tables. `Projects Limit 0`
   forces branch-only use: branch `forgevcs-ci` of `CulebraluxeData`, `aws-us-east-2`
   (ADR-0001 §7.3). A branch is a full copy-on-write instance with its own endpoint and
   object-storage namespace, so isolation holds.
3. **Docker daemon** for T-external (Cockroach + MinIO), if we want the third tier before
   Neon exists at all.

