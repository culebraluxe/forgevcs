# ADR-0001 — ForgeVCS V0 infrastructure stack

- **Status:** Accepted — binding for this workstream
- **Date:** 2026-10-16
- **Authority:** Carter (product owner). Restated here verbatim so any agent or
  human who opens this repo inherits the same constraints without needing the
  original chat transcript.
- **Scope:** ForgeVCS M0 and every workstream after it, until a later ADR
  supersedes it.

This file is a constraint, not a suggestion. If an implementation step appears
to require deviating from it, **stop and write the next ADR** (or ask) — do not
substitute infrastructure.

---

## 1. Approved stack

Use the following stack and **do not substitute alternatives without approval**:

| Concern | Decision |
| --- | --- |
| **Runtime** | Rust + Axum + Tokio |
| **Deployment** | **Separate Vercel project** under the existing Vercel Pro team |
| **Packaging** | Standard Docker container / conventional Rust binary; keep it portable, avoid Vercel-specific business logic |
| **Metadata database** | **Neon Postgres** |
| **Object storage** | **Neon Object Storage**, via its S3-compatible interface |
| **Database access** | SQLx |
| **Content hashing** | Preserve Scion's existing BLAKE3-256 implementation |
| **Compression** | Preserve Scion's existing zstd implementation |
| **Source-control engine** | `CarterT27/scion` Rust crates; **adapt** them rather than rewriting them |
| **Server architecture** | One **modular monolith** initially, not microservices |
| **External service boundaries** | Exactly one `SourceControlService` and one `WorkspaceService` |
| **Security** | Preserve Scion native security **and** add CulebraLuxe security behind an A/B authorization adapter; do not replace either yet |
| **Agent/API access** | REST initially; MCP only after the hosted core is proven |
| **Canonical state** | Neon Postgres + Neon Object Storage **only** |
| **Local filesystem** | Cache/workspace only; **never authoritative** |
| **GitHub** | Temporary bootstrap/mirror/backup only — **not** the target source-control architecture |

### Forbidden unless a measured requirement proves necessity

Railway · AWS · GCP · Kubernetes · Redis · Kafka · NATS · Elasticsearch ·
separate worker services · another database.

"AWS" means *AWS as a platform*. Neon Object Storage is reached with an S3 API
and AWS-shaped credential names (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`);
that is an interface detail, not adoption of AWS as our platform.

## 2. Target deployment shape

```text
Vercel
├── existing CulebraLuxe project
└── separate ForgeVCS Rust/Axum project
         │
         ├── Neon Postgres
         └── Neon Object Storage
```

## 3. Immediate goal (M0)

**ForgeVCS M0:** Carter Scion running behind our Rust service boundary, using
Neon for metadata and objects, independently deployed on Vercel, with the
**upstream conformance tests still passing**.

M0 is not "a rewrite that resembles Scion". It is Scion — its crates, its
hashing, its compression, its storage semantics — reachable through our service
boundary and backed by Neon instead of CockroachDB + MinIO.

---

## 4. Platform facts verified against primary docs

Checked 2026-10-16, because three of these decide whether the approved shape is
*buildable at all*.

| Claim | Verdict | Source |
| --- | --- | --- |
| Rust is an official Vercel runtime | **Yes, but request-scoped:** "The Rust runtime takes an entrypoint of a Rust function using the `vercel_runtime` crate and compiles it into a Vercel Function." | [Functions → Runtimes](https://vercel.com/docs/functions/runtimes) |
| Vercel runs a standard container image | **Yes, Beta, all plans:** "Vercel Functions can run any code and library using Open Container Initiative (OCI) container images, built from a `Dockerfile.vercel` file in your project." Only that exact filename (`Dockerfile.vercel` / `Containerfile.vercel`) is honoured. | [Container Runtime](https://vercel.com/docs/functions/runtimes/container) |
| The container may keep a listening HTTP server | **Yes:** "Functions running container images are expected to open an HTTP server to receive traffic on. The default port is `80`, and it can be overridden by setting the `PORT` environment variable." | [Container Runtime](https://vercel.com/docs/functions/runtimes/container) |
| A container stays warm indefinitely | **No:** "Functions not receiving any traffic for 5 minutes in production environments, and 30 seconds in preview environments, will automatically scale down." Scale-down delivers `SIGTERM` with a **30 s** grace period. | [Container Runtime](https://vercel.com/docs/functions/runtimes/container) |
| Long-running processes are available | **Yes, separately:** Vercel **Daemons** (public beta) "run long-running background processes from container images". Beta limits: production deployments only, manual instance counts, ephemeral storage; no previews, no autoscaling, no persistent storage. | [Daemons](https://vercel.com/docs/daemons) |
| Function limits | Pro/Enterprise: **4 GB** memory; **300 s** default, **800 s** max, **1800 s** extended max (beta) duration; **1,024 file descriptors shared across concurrent executions**; single region by default (`iad1`). | [Functions → Limits](https://vercel.com/docs/functions/limits) |
| Neon Object Storage exists, S3-compatible | **Yes:** "S3-style object storage that lives in your backend, so your files branch and fork copy-on-write with your data." Point "the AWS SDKs, boto3, the AWS CLI, or the Files SDK at your branch endpoint." | [Object Storage](https://neon.com/docs/storage) |
| Object Storage addressing / signing | **Path-style only** ("Virtual-hosted-style requests (`bucket.host/key`) are not supported"); **SigV4 only** (SigV2 unsupported). | [S3 compatibility](https://neon.com/docs/storage/s3-compatibility.md) |
| Object Storage credentials | Injected as `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_ENDPOINT_URL_S3`, `AWS_REGION`. The endpoint is **branch-scoped**, e.g. `https://br-winter-pond-aptw82ef.storage.c-2.us-east-2…` | [Get started](https://neon.com/docs/storage/get-started.md) |
| Object Storage limits | Max object **5 GiB** (single or multipart); throttling returns **`503 SlowDown`** → back off and retry; versioning and lifecycle rules are stored but **not enforced**; **no** bucket notifications. | [Object Storage](https://neon.com/docs/storage), [S3 compatibility](https://neon.com/docs/storage/s3-compatibility.md) |
| Object Storage regions | Only `aws-us-east-2` (Ohio), `aws-us-east-1` (N. Virginia), `aws-eu-central-1` (Frankfurt), `aws-ap-southeast-1` (Singapore). | [Object Storage](https://neon.com/docs/storage) |
| S3 operations we depend on but could **not** confirm in docs | **`CopyObject`** (needed by `S3BlobStore::copy`, `crates/scion-store-s3/src/store.rs:174`) is not visible in the published supported/unsupported tables — the docs page truncates its object-operation table. Treat as **unverified**, not as supported. | [S3 compatibility](https://neon.com/docs/storage/s3-compatibility.md) |

**Consequence for project placement:** Object Storage pins the Neon project to
one of four AWS regions, and Vercel Functions default to `iad1`. The region is
effectively a one-way choice for a project's life — decide at creation.

**What actually happened (2026-10-16):** the only available project
(`CulebraluxeData`) already lived in **`aws-us-east-2`**, and `Projects Limit 0`
ruled out creating an `iad1`-adjacent one. So compute (`iad1`) and storage
(Ohio) cross regions by about 10 ms per round trip. Accepted for now because the
service's hot path is Postgres metadata, not object bytes. If a project slot
frees up, prefer **`aws-us-east-1`** so every hop stays in-region; the migration
is the same single variable as in §7.3.

---

## 5. Consequences for the approved stack

1. **Container Runtime, not the Rust function runtime.** The approved packaging
   is "standard Docker container", which Vercel honours via `Dockerfile.vercel`
   (Container Runtime, beta) — the only mode where a normal `axum`/`tokio`
   server process is the entrypoint. The `vercel_runtime` Rust *function* would
   force request-scoped handlers and abandon the portable binary. The filename
   is load-bearing.

2. **The M0 public surface is HTTP/1.1 REST.** Upstream `scion-server` is a
   **tonic gRPC** server (`crates/scion-server/src/app.rs:386`,
   `tonic::transport::server::Router`). Vercel's container contract is an HTTP
   server on `$PORT` behind its routing layer; gRPC/HTTP-2 framing to a
   container function is not a documented capability. M0 therefore exposes the
   `SourceControlService` / `WorkspaceService` boundaries over REST (which is
   also the approved "REST initially"), and gRPC stays internal/local — never
   Vercel-facing.

3. **No resident background loops inside the container function.** 5-minute
   idle scale-down plus a 30 s `SIGTERM` grace period means retry, lease and
   purge sweeps must be request-triggered, Cron-scheduled, or Daemons. Do not
   design a worker thread the platform will silently kill.

4. **Connection budget is small.** 4 GB RAM and 1,024 shared file descriptors
   per instance cap both the Postgres pool and the S3 client pool. Size pools
   explicitly; never open a connection per request.

5. **5 GiB per object.** Treat it as a hard M0 ceiling for blobs and packfiles;
   larger objects need a chunking decision later (a new ADR).

---

## 6. How the approved stack maps onto the Scion codebase

Established by the W0 baseline (`docs/BASELINE.md`; upstream at
`e0ffadf9c755f1b99812df5beffcabdebcb38c19`, tagged
`FORGESC-SCION-UPSTREAM-BASELINE`).

| Approved decision | Existing Scion capability | Work required |
| --- | --- | --- |
| **Neon Object Storage** | `scion-store-s3` is a `BlobStore` over `object_store`'s `AmazonS3` client | **Configuration only.** `S3Config.path_style = true` (mandatory — Neon is path-style), `allow_http = false`, region from the approved list. No new crate. |
| **Neon Postgres** | `scion-store-crdb` already speaks the Postgres wire protocol (`tokio-postgres` + `deadpool-postgres` + `tokio-postgres-rustls`) | **Port the vendored crate behind a `Dialect`** (feature `postgres`), keeping `tokio-postgres`; SQLx applies to ForgeVCS-owned tables only. See §7.1. |
| **REST first** | `scion-obs::health_router` already returns `axum::Router` serving `/metrics`, `/healthz`, `/readyz` | Thin additions in the ForgeVCS layer; **do not rename the upstream `z`-suffixed routes**. |
| **Conformance stays green** | `crates/scion-store/src/conformance.rs` is already a library: `meta_store_suite(factory)` (line 1496), `blob_store_suite(factory)` (line 1685) | Point the suites at Neon through their factory arguments. No suite modification needed. |
| **Preserve BLAKE3-256 + zstd** | `scion-object`, `scion-core` | Untouched by design. |

### Divergences CockroachDB → Neon Postgres (settle before writing W2 code)

1. **Follower reads.** `crates/scion-store-crdb/src/store.rs:49` uses
   `AS OF SYSTEM TIME follower_read_timestamp()` — no Postgres/Neon equivalent.
   `Consistency` must map to something real (strong read, or read against a
   replica endpoint) without weakening documented semantics.
2. **`UPSERT`.** `crates/scion-store-crdb/src/store.rs:80` uses CockroachDB's
   `UPSERT`; Postgres needs `INSERT … ON CONFLICT DO UPDATE`, column by column.
3. **Isolation level.** CockroachDB defaults to `SERIALIZABLE`; Neon Postgres
   defaults to `READ COMMITTED`. The CAS update of `main` plus idempotent
   retries require `SERIALIZABLE` (or explicit locking) **and** a retry on
   `40001 serialization_failure`. Without that, W2 acceptance is not met — a
   lost update is exactly the failure the conformance suite exists to catch.

---

## 7. Open decisions requiring Carter's ruling (7.1 resolved 2026-10-16)

### 7.1 SQLx vs. the existing `tokio-postgres` store — **RESOLVED: port, do not rewrite**

**Carter's ruling (2026-10-16):** *"if sql is sql then just port it; if there are
advantages to port it as Postgres then I pay the tokens to do it right."*

Having read the backend, the honest answer is that **the expensive part is already
written**, so porting is the way to "do it right" — and a SQLx rewrite would spend
tokens to re-derive correctness that already exists:

| Evidence | Finding |
| --- | --- |
| `scion-store-crdb/src/store/txn.rs:161` | `BEGIN ISOLATION LEVEL SERIALIZABLE` is **already** the transaction shape. The isolation divergence from ADR §6.3 is not open work |
| `store.rs:101`, `store/txn.rs:74` | A **bounded `40001` retry loop already exists** (`retried attempts` counter; `40003` treated as retryable only for read-only methods). ADR §6.3's "retry on serialization failure" is already implemented |
| `store.rs:80-88`, `store.rs:1316` | Only **three** `UPSERT` statements (`counters.redaction_epoch`, `counters.audit_seq`, `derive_leases`) → three `INSERT … ON CONFLICT DO UPDATE` rewrites |
| `store.rs:49` | Exactly **one** CockroachDB-only construct: `AS OF SYSTEM TIME follower_read_timestamp()`, used **only** for `Consistency::Stale` |
| `src/migrations.rs` | 14 tables; dialect surface is mechanical but wide: **`BYTES` ×24 → `BYTEA`**, **`STRING` ×21 → `TEXT`**, `INT8` ×26 and `TIMESTAMPTZ` ×12 already valid in Postgres |
| `src/store.rs` size | 1,637 lines, of which the divergence surface is **4 statements + 45 type names** |

So "SQL is not quite SQL": the *syntax* is a find-and-replace, and the *semantics* that
make a source-control store correct (serializable CAS, bounded retry) are already there
and already exercised by `meta_store_suite`.

**Decision: port, in the vendored crate, behind a dialect switch.** No second `MetaStore`
implementation, no re-derivation of the transaction machinery.

- Add `src/dialect.rs` to the vendored `scion-store-crdb` with a `Dialect::{Cockroach,
  Postgres}` selected by a cargo feature (`postgres`). Each divergent statement becomes
  one `match`; `migrations.rs` renders Postgres type names. One crate, two dialects, one
  test suite.
- Cost accepted: the migration port is 14 tables of mechanical type renaming, and it must
  be *proven* by `meta_store_suite` against Neon, not by inspection.
- Open sub-decision inside this ruling: `Consistency::Stale` has no Neon equivalent to
  follower reads. Options are (a) serve `Stale` as a strong read — correct, but silently
  drops the latency benefit the variant exists for; or (b) hold a second pool against a
  Neon **read replica** endpoint. Decide during W2 with a measurement; do not guess now.

**How this sits with the SQLx directive.** The stack table's `Database access: SQLx` is
applied to **ForgeVCS-owned tables and the API layer** (new code, new tables, SQLx with
`sqlx::migrate!`). The **vendored Scion store stays on `tokio-postgres`**, because
rewriting it is precisely the "rewrite rather than adapt" the same document forbids, and
because `40001`-retry correctness already lives there. This is an interpretation of two
clauses that tension each other — flag it back to Carter rather than letting it pass
silently.


#### 7.1 outcome — W2 ported and proven (2026-10-17)

Implemented exactly as decided, in the vendored crate, behind `--features postgres`. The
default build still emits the upstream CockroachDB statements; `cargo fmt --check` is clean
and both builds' unit tests are 15/15.

**Acceptance evidence** (unpooled Neon URL, `us-east-2`):

| Check | Result |
| --- | --- |
| `meta_store_suite` against Neon (`tests/neon_conformance.rs`) | **20/20 cases**, 141 s |
| Includes 32-racer `concurrent_cas` / `concurrent_inserts` / `concurrent_leases` / `concurrent_redactions` | pass |
| Unit tests, default (CockroachDB) build | 15/15 |
| Unit tests, `--features postgres` | 15/15 |
| Test isolation without `CREATE DATABASE` per case | one database, one `repo_name`→`repo_id` per case |

**Sub-decision resolved: `Consistency::Stale` serves a strong read on Postgres.** Option (a)
from the list above. No replica pool: the stale variant exists for latency, not correctness,
and a second pool against a read replica doubles the credential and endpoint surface for a
benefit that the acceptance suite cannot measure. The enum and its contract are unchanged;
only the hint is dropped, and `stale_variants_are_the_strong_statement` pins that.

**What the pre-work analysis missed** — found by running the suite, not by reading:

1. **`optimizer_use_forecasts` is CockroachDB-only.** Set on every connection, it makes every
   Neon connection fail with `unrecognized configuration parameter`. Now
   `Dialect::session_options()`.
2. **`audit_log.seq DEFAULT unique_rowid()`** has no Postgres equivalent; it becomes
   `GENERATED BY DEFAULT AS IDENTITY` (inserts still pass `seq` explicitly, so behaviour is
   unchanged).
3. **More `::BYTES[]`/`::STRING[]` casts than the two batched inserts.** Five further
   statements (`objects`, `object_uploads`, `changeset_children`, `object_paths`, `landed`,
   `redactions`) had them inline. Rather than duplicate strings, `adapt_inline_sql` rewrites at
   the three statement-preparation helpers — the one place all SQL passes through.
4. **The 10-attempt retry budget is a CockroachDB number.** A 32-racer case against SSI
   exhausts it (`concurrent_redactions` failed once, then passed once the budget became
   dialect-scoped: 10 for CockroachDB, 64 for Postgres). This is the finding that would have
   shipped as an intermittent 503 under load.
5. **The test harness assumed plaintext.** `connect_fresh_for_tests` used `NoTls` and
   `CREATE DATABASE` per case; Neon rejects both (~100 databases per branch, TLS mandatory).
   It now takes TLS from `dialect::test_tls()`, and the Neon run isolates by repository name
   instead of by database.

### 7.2 `CopyObject` on Neon Object Storage — RESOLVED: supported

`S3BlobStore::copy` calls `object_store`'s `copy`, i.e. S3 `CopyObject`, and
Neon's compatibility page never confirmed it. Verified **2026-10-17** against
bucket `forgevcs-objects` on branch `forgevcs-ci`, using raw SigV4 requests
(curl `--aws-sigv4`; no `aws` CLI on this machine):

| Probe | Result |
| --- | --- |
| `PUT` 14-byte object | 200 |
| `PUT` zero-byte object | 200 |
| `HEAD` existing object | 200 |
| **`CopyObject`** `probe/a.txt` → `probe/b.txt` | **200**, `CopyObjectResult`, ETag = source ETag |
| `GET` the copy | 200, bytes identical to source |
| `ListObjectsV2` on `prefix=probe/` | 200, `KeyCount` 3 |

So no `GetObject`+`PutObject` emulation is needed and `S3BlobStore::copy`
stands as written. Probe objects were deleted afterwards. `blob_store_suite`'s
`copy_delete` case still runs in W3, but as an in-tree regression rather than
an open platform question.

### 7.3 Which Neon project/branch

Never the CulebraLuxe production database — the conformance suite creates and
drops tables. **Actual outcome (2026-10-16):** `neon me` reports
`Projects Limit 0`, so a project of our own was not possible; the suites run on
branch **`forgevcs-ci`** (`br-muddy-wind-ax2iknwx`) of project `CulebraluxeData`
(`snowy-salad-48970537`), region **`aws-us-east-2`**. That is acceptable because
a Neon branch is a full copy-on-write Postgres instance with its own endpoint and
its own object-storage namespace — the branch, not the project, is the isolation
boundary. If Carter frees a project slot later, migration is one variable
(`FORGEVCS_DATABASE_URL`); nothing else in the design depends on the choice.

---

## 8. How to use this document

- Any agent working in this repo treats §1 as immutable input. New
  infrastructure is a **new ADR**, not a quiet substitution.
- Work proceeds **WIP=1**: one workstream (`W1`, `W2`, …) in progress at a time;
  the next does not start until the previous acceptance gate passes.
- Every workstream's acceptance includes: upstream `scripts/check.sh` still
  green, and the relevant conformance suite green against Neon.

---

## 9. Platform releases verified 2026-10-16 (Vercel + Neon)

Added after Carter's note that *"Vercel has new containers this week"*. All three claims
are **real**; two details in the circulating summary were wrong, and are corrected below.

| Release | Date | Verified text | Consequence for ForgeVCS |
| --- | --- | --- | --- |
| Container images in Functions — *"Bring your Dockerfile to Vercel Functions"* | **30 Jun 2026** | "Vercel Functions now support deploying HTTP servers from a Dockerfile or Containerfile, using Open Container Initiative (OCI) compatible images on Fluid compute." … "create a project with a `Dockerfile.vercel` (or `Containerfile.vercel`) file that starts an HTTP server listening on `$PORT`" | This is exactly the shape §2 already assumes. A plain `cargo build --release` binary in a slim image is a first-class deployment unit — no Vercel-specific handler, no `vercel_runtime` crate |
| **Vercel Daemons** — public beta | **15 Oct 2026** | "run long-running processes that pull and execute work from Vercel Queues, databases, or third-party systems without requiring an HTTP server" … "Daemons run containers on Fluid compute and deploy alongside your app in the same project" … "maintains the instance count you configure in each region, restarting instances that exit or crash" | Declared by a `daemons` entry in `vercel.toml` / `vercel.json` / `vercel.ts`. **Not used in V0**: one Axum container is the whole system until a measurement says otherwise |
| **Vercel Drives** — GA | **15 Oct 2026** | "now generally available for Vercel Sandbox on Hobby, Pro, and Enterprise. A Drive is persistent storage that you mount as a directory in a Vercel Sandbox." … "You can also fork a Drive into independent writable Drives." | **Sandbox-only.** A Drive cannot be mounted into the ForgeVCS HTTP container, so it can never hold canonical state. Optional later, for agent workspaces |

### 9.1 Consequences

1. **Nothing in §1 changes.** Canonical state stays Neon Postgres + Neon Object Storage.
   A Drive is a working copy; losing or corrupting one must never lose source history.
2. **Portability is the point.** The service stays a boring `cargo build` → Docker →
   `$PORT` binary, so leaving Vercel is a deployment change, not a rewrite. No Vercel SDK
   calls in business logic; the only Vercel-specific artefact is the filename
   `Dockerfile.vercel`.
3. **Corrections to the circulating summary.** (a) The 30 Jun changelog does **not**
   mention Rust or Axum — its example is Go — so the "official Rust/Axum Docker deployment
   guide" claim is **unverified**; do not cite it as fact. (b) Drives being forkable is
   real, but forkability is a Sandbox feature, not a ForgeVCS branch-model feature (§9.2).

### 9.2 Three different things called "fork" — do not conflate them

| Kind | Mechanism | Owner / permitted use |
| --- | --- | --- |
| ForgeVCS changesets and refs | Scion objects in Neon Postgres | **our product** — the only real repository model |
| Neon branch | copy-on-write Postgres **and** bucket state | dev / test / preview / DR only |
| Vercel Drive fork | copy-on-write Sandbox filesystem | agent workspace only |

A Neon branch must never represent a repository branch, and a Drive fork must never be the
only copy of anything.

### 9.3 Neon infrastructure-as-code

Neon ships `neon.ts` (announced 15 Jun 2026) with a CLI-driven flow
(`neon link` → `neon config init` / `neon deploy` → `neon env pull`) that
provisions buckets and writes the `AWS_*` variables. The installed CLI here is
**3.6.0**. Verified 2026-10-17: the flow works, but it is **not required**.
`neon link --project-id … --branch …` → `neon buckets create <name>` →
`neon env pull` provisions a bucket with no `neon.ts` and no `neon deploy`, and
that is the path this repo uses. Two facts from the live run matter more than
the tooling: buckets live in the config as `preview.buckets` (not a top-level
`buckets` key), and both the bucket and its S3 endpoint are **branch-scoped**
(`https://<branch-id>.storage.<cell>.<region>.aws.neon.tech`) — so the branch,
not the project, is the unit of isolation. Details in
[`../W3-object-storage.md`](../W3-object-storage.md) §3.




