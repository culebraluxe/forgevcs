# W3 — Neon Object Storage work order: what is already done, what is actually left

**Status: code landed and building (2026-10-17); Neon run pending.** Provisioning is no
longer blocked (see §7). Work order received 2026-10-16 (Carter). This file maps each clause
of that order onto the vendored Scion source so we do not rebuild a storage layer that
already exists, then records the ForgeVCS delta.

Upstream at `e0ffadf9` (`FORGESC-SCION-UPSTREAM-BASELINE`). All line references below were
read from the source, not inferred.

## 1. Clause-by-clause: already satisfied by Scion

| Work-order clause | Already implemented | Where |
| --- | --- | --- |
| §2 provider-independent object abstraction | `BlobStore` trait: `put`, `get`, `copy`, `delete`, `exists`, `list`, `&str` keys, `Bytes` values | `crates/scion-store/src/blob.rs:10-23` |
| §2 S3-compatible implementation | `S3BlobStore` over `object_store` `AmazonS3` (`object_store = "=0.12.5"`, feature `aws`) | `crates/scion-store-s3/src/store.rs`; `Cargo.toml:125` |
| §4 content-addressed key layout with 2-char fan-out | `{prefix}/{area}/{repo_id}/{kind}/{fan2}/{hex}`, `area` = `r` (live) or `q` (quarantine), kind letters incl. `x<kk>` for derived | `crates/scion-store/src/keys.rs:11-22` |
| §4 immutable objects, no semantic names | Every key derives from an `ObjectId`; no path-like or mutable naming exists in the store | same |
| §5 compute BLAKE3 before upload | `put_blob` → `ObjectId::hash_blob(&raw)` | `crates/scion-store/src/repo.rs:311-319` |
| §5 zstd compression | `cfg.zstd_level` (default **3**), `zstd::bulk::compress`, inline threshold **65,536** B | `repo.rs:36-56`, `repo.rs:262-275` |
| §5 idempotent upload | `InsertOutcome::AlreadyExists` for a duplicate id; bytes-before-row ordering documented | `repo.rs:1-7`, `repo.rs:367-400`; asserted in `conformance.rs:238,278,473` |
| §6 fetch → decompress → recompute BLAKE3 → compare | `load_verified` runs `digest_matches` after decompression | `repo.rs:596-645` |
| §6 hard integrity error, never corrupt bytes | `integrity_alert`: increments `record_integrity_failure`, emits `ERROR` event `integrity_alert`, returns `ScionError::integrity(id)` | `repo.rs:231-240` |
| §7 Postgres holds metadata only | `objects` row carries `id`, `kind`, `size`, `stored_size`, `inline`; bytes go to object storage (or inline ≤ threshold). No content column for large objects | `repo.rs:262-308`; schema `crates/scion-store-crdb/src/migrations.rs:16` |
| §8 compression round trip, corrupt rejection | `corrupt_bytes_give_integrity_and_alert` plus zstd round trip | `crates/scion-store/tests/store_repo.rs:557` |
| §8 concurrency | `blob_store_suite` case `concurrent`; `meta_store_suite` covers concurrent CAS | `conformance.rs:1685-1699` |
| §8 "run all existing conformance tests against the adapter" | `blob_store_suite(factory)` — factory-based, **no suite edits needed** | `conformance.rs:1685`; call site `crates/scion-store-s3/tests/s3_external.rs:91` |
| §10 "local disk not authoritative" | Enforced by layering: `scion-cache` is a cache; the layer check forbids upward dependencies | `scripts/check-layers.sh` |

**Conclusion:** the work order describes a storage layer Scion already has. The remaining
work is *configuration + verification*, not authoring `put`/`get`/`exists`.

## 2. Where the work order must bend to Scion (do not "fix" these)

1. **Key layout.** The order suggests `objects/<first-two>/<full-hash>`. Scion's is
   `{prefix}/{r|q}/{repo_id}/{kind}/{fan2}/{hex}` (SPEC 5.1). Adopting the order's layout
   would break area separation — `quarantine` is how redaction and re-introduction blocking
   work — and would contradict the SPEC. **Keep Scion's.**
2. **Trait shape.** The order sketches `put/get/exists(&ObjectHash)`. Scion's `BlobStore`
   takes `&str` keys and also needs `copy`, `delete`, `list`. Narrowing it at the ForgeVCS
   boundary would lose `copy` (quarantine moves) and `list` (GC). **Adapt at the edge.**
3. **"Bucket private".** Neon buckets are private by default; public is the opt-in
   (`access: 'public_read'`). `access: 'private'` is not a documented value — omit it (§3).
4. **`copy` = S3 `CopyObject`.** `S3BlobStore::copy` calls `object_store.copy`
   (`store.rs:174`), and Neon's published compatibility table does not confirm
   `CopyObject`. This is a **real risk to quarantine/redaction**, not a detail. Resolve it
   by running `blob_store_suite` (case `copy_delete`) against a real Neon bucket before
   claiming §10.
5. **`Consistency::Stale`** (follower reads) has no Neon equivalent — see ADR-0001 §7.1.

## 3. Neon provisioning: verified commands, and the two traps

Verified 2026-10-16/17 against the Object Storage product page, the `buckets` CLI
reference, and a live bucket on the `forgevcs-ci` branch.

| Item | Verified fact |
| --- | --- |
| CLI | `neon` **3.6.0** at `/opt/homebrew/bin/neon`; has `auth`, `link`, `deploy`, `env pull`, `config status/plan/apply/init`, `buckets` |
| Config file | `neon.ts` at project root, `defineConfig({...})` — **not used by this repo**; CLI flags suffice |
| **Trap 1 — package name (settled)** | The product page imports `defineConfig` from **`@neon/config/v1`**, so the 15 Jun `@neondatabase/config` spelling was the announcement's error. Moot anyway: we do not use `neon.ts` (see Trap 2) |
| **Trap 2 — buckets shape (was wrong here)** | An earlier revision of this doc claimed buckets were a *top-level* `buckets` key. The product page shows them nested: `preview: { buckets: { uploads: {} } }`. Do not hand-write either shape — provision with the CLI, which needs no `neon.ts` at all |
| Provisioning (used) | `neon link --project-id <id> --branch <branch>` → `neon buckets create <name> --project-id <id> --branch <branch>`; **no `neon deploy`, no `neon.ts`** |
| Re-pull | `neon env pull` (non-interactive once linked) |
| Env vars | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_ENDPOINT_URL_S3`, `AWS_REGION` (+ `DATABASE_URL`, `DATABASE_URL_UNPOOLED`, `NEON_AUTH_*`, `NEON_AI_GATEWAY_*`) |
| Endpoint shape | Branch-scoped: `https://<branch-id>.storage.<cell>.<region>.aws.neon.tech`; **path-style only**, **SigV4 only** |
| Object ops | `neon buckets object put/get/list/delete`, `--recursive` for prefixes |
| Credentials | Scoped credentials shared with AI Gateway, issued/rotated by `neon env pull` (it revoked its own predecessor on second run) |

**Live state 2026-10-17.** Bucket `forgevcs-objects` (**private**) exists on branch
`forgevcs-ci` (`br-muddy-wind-ax2iknwx`), project `snowy-salad-48970537`,
region `us-east-2`. Provisioned entirely from the CLI after `neon auth` was fixed;
object PUT / zero-byte PUT / HEAD / **CopyObject** / GET / ListObjectsV2 all returned
200 with raw SigV4 requests — see ADR-0001 §7.2. Probe objects deleted.

**Branch coupling is architectural, not incidental.** Buckets branch with the
database (copy-on-write, isolated namespaces, deleted with the branch), and the S3
endpoint is *per branch*. So `FORGEVCS_BUCKET` names a bucket that exists on whatever
branch `AWS_ENDPOINT_URL_S3` points at; a deploy that pins one branch's endpoint cannot
read another's objects. Vercel gets the four `AWS_*` vars, never `DATABASE_URL_UNPOOLED`.

## 4. Rust-side configuration (the actual new code)

`S3Config` must be built from the environment, with no committed secrets:

```text
bucket     = FORGEVCS_BUCKET       (required; `forgevcs-objects` is the bucket §3 provisions)
endpoint   = AWS_ENDPOINT_URL_S3
region     = AWS_REGION
access     = AWS_ACCESS_KEY_ID
secret     = AWS_SECRET_ACCESS_KEY
path_style = true     ← mandatory; Neon rejects virtual-hosted style
allow_http = false    ← Neon is HTTPS-only
key_prefix = FORGEVCS_KEY_PREFIX  (optional; maps to S3Config.key_prefix)
```

`S3Config` already rejects empty values (`store.rs:52-108`) and applies `path_style`.
This is the entire ForgeVCS delta for clause §3 of the work order.

## 5. Tests: what exists, what must be added

`blob_store_suite` runs four cases (`conformance.rs:1690-1693`): `put_get`,
`copy_delete`, `list`, `concurrent`.

| Work-order case | Status |
| --- | --- |
| put new object, get existing | covered (`put_get`) |
| concurrent reads/writes | covered (`concurrent`) |
| corrupt object → hard integrity error | covered, at the object-repo layer: `store_repo.rs:557` |
| identical object uploaded twice | repo layer covered (`InsertOutcome::AlreadyExists`, `conformance.rs:238`); blob layer added in W3 — `blob_store_suite_extended` case `duplicate_put` (§8) |
| missing object | covered explicitly in W3 — case `missing_object` (§8) |
| **zero-byte object** | added in W3 — case `zero_byte` (§8) |
| **large object** (above the inline threshold; 12 MiB, past the S3 multipart threshold) | added in W3 — case `large_object` (§8). The repo layer's own 100 MiB `max_blob_bytes` ceiling is `store_repo.rs:354` |

So the honest test delta was **three additions** — explicit blob-level duplicate `put`,
zero-byte, and large-object cases — plus running the existing suites against Neon. W3 landed
those as a separate `blob_store_suite_extended`, which the in-memory backends run too.

## 6. Acceptance mapping (work order §10)

| Criterion | How it will be proven |
| --- | --- |
| Rust can PUT into Neon Object Storage | `blob_store_suite` case `put_get` against the Neon bucket |
| Rust can GET it back | same case |
| Hash verification succeeds | `store_repo` integrity cases (`store_repo.rs:557`) |
| Duplicate writes safe | `AlreadyExists` assertions + new blob-level duplicate case |
| Malformed/corrupt data rejected | `integrity_alert` + failure counter |
| No credentials committed | `.env.local` git-ignored; config read from env only |
| Scion conformance tests remain green | `scripts/check.sh` (T-core) exit 0 |
| Postgres contains metadata only | `objects` has no content column; large blobs live in object storage |
| Local disk not authoritative | layering check + no cache write path in the store crate |

## 7. Sequencing (WIP=1)

Per ADR-0001 §8, code work is serialized: **W1 (server shell) → W2 (Postgres port) → W3
(object storage)**. Provisioning is not code and can proceed in parallel once
authentication works, so the unblocking order is:

1. ~~Carter runs `neon auth`; confirms/creates a project.~~ **Done** — auth works
   (`culebraluxe`); region is `aws-us-east-2`; the CI branch is `forgevcs-ci`. See
   ADR §7.3 for why it is a branch and not a project of its own.
2. ~~`neon link` → `neon config init` → `neon deploy` → `neon env pull`~~ **Done, shorter:**
   `neon link --project-id … --branch forgevcs-ci` → `neon buckets create forgevcs-objects`
   → `neon env pull`. Variables land in `.env.local`, which is gitignored.
3. ~~Run `blob_store_suite` against it **before** writing adapter code~~ **Platform half
   answered 2026-10-17** by raw SigV4 probes: `CopyObject` returns 200 (ADR §7.2). The
   suite run still happens in W3 — now as regression, not discovery.

## 8. What W3 added (2026-10-17)

Four additions, no adapter rewrite. The vendor delta stays additive: one new suite, two new
tests, no change to any existing case.

| what | where | why |
| --- | --- | --- |
| `blob_store_suite_extended` | `upstream/crates/scion-store/src/conformance.rs` | §5's three missing cases — explicit duplicate `put`, zero-byte, and a value past the multipart threshold (12 MiB) — plus a missing key asserted across the whole read path. A separate entry point so `blob_store_suite` stays byte-comparable with the W0 recording. |
| the same suite on the in-memory backends | `upstream/crates/scion-store/tests/store_conformance.rs` | the new cases are proven hermetically (raw and instrumented), so a failure against Neon is a backend or config problem, not a broken case |
| `Config::s3_config()` | `crates/forgevcs-service/src/config.rs` | §4's client settings from the environment: `path_style = true` (Neon rejects virtual-hosted style), `allow_http = false`, credentials passed explicitly so an ambient `AWS_*` profile cannot take over, and a half-configured key pair rejected by `validate()` |
| `neon_blob_conformance` | `crates/forgevcs-service/tests/neon_blob_conformance.rs` | §6's acceptance: both suites plus one round trip at the real key layout through two independent clients (strong read-after-write), built from the *service's* `Config` |

The crate became a **library plus a thin binary** (`src/lib.rs` exposes `config`; `main.rs`
keeps the router) purely so an integration test can drive the same configuration code the
container runs. The deployed binary is unchanged.

Acceptance command — hermetic first, then the bucket:

```sh
cargo test -p forgevcs-service --all-targets          # skips the Neon tests when unconfigured
set -a; . ./.env.local; set +a
cargo test -p forgevcs-service --test neon_blob_conformance -- --nocapture
```

Every run writes under its own key prefix (`w3-<nanos>-<case>` under `FORGEVCS_KEY_PREFIX`),
so repeated or concurrent runs cannot see each other's objects, which is also the suite's
"fresh empty store per case" precondition. Nothing needs cleaning up first.

### Result (2026-10-17)

`cargo test -p forgevcs-service --test neon_blob_conformance` against the `forgevcs-ci`
branch's bucket: **3 passed / 0 failed, 28.62 s** — the base suite (4 cases), the extended
suite (duplicate put, zero byte, 12 MiB multipart, missing object), and the cross-client
round trip at the real key layout. No `SKIP:` lines, so nothing was silently gated off.

The same suites run on the in-memory backends (`store_conformance`, **7/7**), which is what
makes a Neon failure diagnosable: the cases themselves are already proven.



