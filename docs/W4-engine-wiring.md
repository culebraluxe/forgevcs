# W4 (parts 1–3) — Engine wiring, the auth seam, and the business façade

**Status:** parts 1–3 complete 2026-10-17. `POST /repos/{repo}/init`, `/push`, `/pull` and
`GET /repos/{repo}/refs`, `/commits`, `/changesets/{id}`, `/file`, `/tree`, `/diff`,
`/merge-base` call the vendored `scion.v1` handlers in-process, after
`interceptor::authenticate` produces the `CallerCtx` every handler reads. The 20-check
`scripts/w4-acceptance.py` client proves the work order's §8 loop (A → B, parent, history,
file versions, tree, diff, merge-base) against Neon. Remaining W4 gaps are recorded in §8.6:
repository deletion, restart recovery for non-default repos, human login, and the gRPC layers
the façade does not route through.

## 1. What this part solves

W0–W3 built and proved the pieces — metadata store on Neon, object store on Neon — but the
running service touched neither. `forgevcs-service` served three probes and nothing else, so
`/ready` returned 200 for a process that could not reach its own database.

This part closes that: startup now boots the real Scion engine, and `/ready` is a statement
about Neon, not about a config struct.

## 2. What changed, part 1 — engine wiring

| Path | Change |
| --- | --- |
| `crates/forgevcs-service/src/engine.rs` | **new.** `Engine::connect` (config → `CrdbMetaStore` → `NeonBlobStore` → `PolicyHandle` → `Keyring` → `Storage` → `ServerApp`), `Engines` registry, `EngineHealth` readiness |
| `crates/forgevcs-service/src/config.rs` | `repo()`, `max_connections()`, `signing_keys()`, `service_principals`, `FORGEVCS_POLICY_PATH`, and the upstream's `FORGEVCS_*`/`AWS_*` alias rules |
| `crates/forgevcs-service/src/main.rs` | boots `Engines` before binding; `/ready` reads the probe snapshot instead of `Config::validate` |
| `crates/forgevcs-service/Cargo.toml` | depends on the vendored `scion-server`, `scion-config`, `scion-store-crdb` (`postgres` feature), `scion-object-neon` |
| `acl.toml` | **new.** Deny-by-default policy: agents only |
| `upstream/crates/scion-store-crdb/src/store.rs` | `CrdbMetaStore::ping()` — `SELECT 1`, the metadata half of `/ready` |

Upstream behaviour is unchanged by `ping()`: it is a new inherent method that opens a
connection from the existing pool and runs one query. The default (CockroachDB) build does
not reference it.

## 3. Boot sequence

1. `Config::from_env()` — one place maps the environment; every value has an alias rule
   (`FORGEVCS_DATABASE_URL` → `DATABASE_URL`, `FORGEVCS_BUCKET` → `NEON_BUCKET`/`BUCKET`).
2. `Engines::boot` — if the database URL or the bucket is missing, record the reason and
   **do not start an engine**. Otherwise:
   `CrdbMetaStore::connect` (migrations run here) → `NeonBlobStore::new` → `PolicyHandle::load`
   → `Keyring` → `Storage::new` → `ServerApp::from_parts`.
3. `EngineHealth::start` — one probe immediately, then every 5 s. The router reads the
   cached result, so `/ready` never awaits Neon.
4. Bind `0.0.0.0:$PORT`, log `repos=1`.

A failure at any step is recorded, logged once at `ERROR`, and surfaces as `/ready` 503 with
that exact reason. The process stays up (ADR-0002 D5) — a container that exits on a Neon blip
restarts forever and tells nobody why.

## 4. Acceptance

Unit/integration (no network):

```sh
cargo fmt --check
cargo clippy -p forgevcs-service --all-targets -- -D warnings   # 0 warnings
cargo test -p forgevcs-service --lib --bins                      # 17 passed
```

Upstream, because `store.rs` was touched (the vendored gate is the contract):

```sh
scripts/gate.sh    # 1343 passed / 0 failed — same count as the W3 baseline
```

Live, against branch `forgevcs-ci` and bucket `forgevcs-objects`:

```sh
scripts/engine-smoke.sh      # sources .env.local, boots the binary on :3999, curls the probes
```

Observed (the script's own output):

```
/health    HTTP 200  {"service":"forgevcs","status":"ok","version":"0.1.0"}
/ready     HTTP 200  {"service":"forgevcs","status":"ready","version":"0.1.0"}
/readyz    HTTP 200  ready
/version   HTTP 200  {"service":"forgevcs","upstream":"e0ffadf9c755f1b99812df5beffcabdebcb38c19"}
```

| Probe | Before W4 | After W4 |
| --- | --- | --- |
| `/health` | 200 `{"status":"ok"}` | 200 `{"status":"ok"}` (still never touches Neon) |
| `/ready` | 200 (config only) | **200** `{"status":"ready"}` with `SELECT 1` + `HEAD` answering |
| `/ready` with no `FORGEVCS_BUCKET` | 200 | **503**, `reason` names the missing variable |
| `/readyz` (Scion's route) | 200 | 200 `ready` |

Service log on a healthy boot:

```json
{"level":"INFO","message":"policy loaded","path":"/…/acl.toml","policy_epoch":682736354533426236}
{"level":"INFO","message":"scion-server built","repo":"forgevcs","policy_epoch":682736354533426236}
{"level":"INFO","message":"engine ready","repo":"forgevcs"}
{"level":"INFO","message":"forgevcs listening","port":3999,"repos":1,"upstream":"e0ffadf9…"}
```

## 5. Findings worth keeping

1. **Neon requires TLS and `CrdbConfig.tls` is `None` by default.** First live boot failed
   with `error performing TLS handshake`. The fix is upstream's own
   `scion_store_crdb::dialect::tls_client_config()` (webpki roots), passed explicitly. Any
   later caller that constructs `CrdbConfig` must do the same.
2. **`Storage::new` takes `Arc<dyn MetaStore>`; `Arc::clone` cannot coerce.** The engine
   keeps the concrete `Arc<CrdbMetaStore>` (for `ping`) and binds a trait-object clone in a
   `let` before calling `Storage::new`.
3. **`acl.toml` needs a `/` rule.** The parser rejects a policy without one; deny-by-default
   is expressed as a root rule with empty lists, not by omitting the rule.
4. **The metadata URL must be the unpooled string.** `.env.local` carries both; the smoke
   uses `DATABASE_URL_UNPOOLED` because migrations and `SELECT 1` run through a direct
   connection.

## 6. Deliberately not in part 1

- No business REST endpoints. The engine is built and reachable in-process and the auth seam
  verifies callers (part 2, §7), but `init`/`push`/`pull` still need the JSON ↔ `scion.v1`
  mapping for the five write/read services.
- No repository creation. `Engines` has one repo from `FORGEVCS_REPO`; `create_repository`
  is a W4 gap that will add a `forgevcs_repos` row plus an engine per name (ADR-0002 D6
  table split).
- No human identities. Policy grants the two agent principals only; W5 adds workspace
  members and W6 the delegation decisions.

## 7. Part 2 — the auth seam

The façades that follow need an authenticated caller, so auth came first: there is no useful
`/push` without a verified `CallerCtx`, and building one `authenticate` call per endpoint
would invite a second authorization implementation.

### 7.1 What changed

| Path | Change |
| --- | --- |
| `crates/forgevcs-service/src/http.rs` | **new.** `AppState` (probes + engines + repo), the router, the probes moved out of `main.rs`, `POST /auth/token`, `GET /auth/whoami`, one error shape, and the `tonic::Status` → HTTP map |
| `crates/forgevcs-service/src/main.rs` | startup, shutdown and `axum::serve` only — it now builds `AppState` and hands it to `http::router` |
| `crates/forgevcs-service/src/lib.rs` | exports `http`, so tests drive the identical router |
| `upstream/crates/scion-server/src/lib.rs` | `mod interceptor;` → `pub mod interceptor;` — the D4 seam was not actually reachable (README modification log, item 8) |
| `crates/forgevcs-service/Cargo.toml` | adds `scion-proto` (the `scion.v1` messages) and `tonic` (`Request`, `Status`, `Code`; no features beyond `=0.14.6`) |

`login` and `who_am_i` are upstream's handlers, called directly. Nothing about the token is
re-implemented: the SHA-256 comparison, the `gid` claim rule, the TTL and the response fields
all come from `handlers::auth`.

### 7.2 Endpoints

```sh
# mint: a configured service principal and its secret
curl -sX POST localhost:3000/auth/token -H 'content-type: application/json' \
  -d '{"service":"maestro","secret":"…"}'
# {"service":"forgevcs","status":"ok","token":"…","principal":"service:maestro","expires_at_unix":…}

# spend: any token the service accepts, on any future façade call
curl -s localhost:3000/auth/whoami -H "authorization: Bearer $TOKEN"
# {"service":"forgevcs","status":"ok","principal":"service:maestro","groups":["agents"],"expires_at_unix":…}
```

The error shape is the probes' shape with `status:"error"`: `{"service","status","code",
"message"}`. `tonic::Status` maps to HTTP once (`status_response`), so the façade and the gRPC
surface cannot disagree about what a failure means.

Tokens live 12 h (`auth.token_ttl`, upstream's SPEC 6.5 default, set explicitly in
`engine.rs`), so a caller that keeps a token past that gets 401 and mints again — there is no
refresh path, and a leaked token expires on its own. The signature only outlives a deploy once
`FORGEVCS_SIGNING_KEY_PATH` points at a real key; with it unset the process generates an
ephemeral key and says so at startup, so every token dies with the process.

### 7.3 Acceptance

```sh
cargo fmt --all --check                                        # clean
cargo clippy -p forgevcs-service --all-targets -- -D warnings  # 0 warnings
cargo test -p forgevcs-service --lib --bins                    # 21 passed
scripts/gate.sh                                                # 1343 passed / 0 failed
```

Live, `scripts/engine-smoke.sh` (the auth lines it now prints):

```
token      minted (242 chars)
whoami     HTTP 200  {"expires_at_unix":…,"groups":["agents"],"principal":"service:maestro",…}
bad-secret HTTP 401  {"code":"unauthenticated","message":"invalid service credentials",…}
no-token   HTTP 401  {"code":"unauthenticated","message":"missing bearer token",…}
```

### 7.4 Findings

1. **`interceptor` was private.** The ADR assumed `scion_server::interceptor::authenticate`
   was public; only `CallerCtx` was re-exported. The fix is one visibility change, not a
   copy of bearer parsing — a copy would be exactly the second auth path D4 forbids.
2. **`tonic` is needed even without gRPC.** Handlers take `tonic::Request`, so the façade
   depends on `tonic` for `Request`/`Status` alone. Pin the same `=0.14.6` upstream uses and
   leave its features off; the graph already enables what the transport needs, and `http`
   stays a single `1.5.0`, so `axum::http::HeaderMap` is the same type the interceptor takes.
3. **`main.rs` is the wrong home for routes.** Once the second endpoint existed, the test
   router and the served router had to be the same object; `http.rs` makes that structural
   rather than a convention.

### 7.5 What part 2 deliberately left

- The business endpoints themselves. Part 2 proved the token loop; part 3 (§8) spends the
  `CallerCtx` on the five operations.
- `create_repository`/`delete_repository` (ADR-0002 D6). `init` now creates a repository, but
  there is no `forgevcs_repos` metadata row and no delete.
- Token issuance for humans: `Login`'s GitHub branch needs `FORGEVCS_GITHUB_CLIENT_ID`; until
  then only service principals can obtain a token.

## 8. Part 3 — the business façade

### 8.1 What changed

| Path | Change |
| --- | --- |
| `crates/forgevcs-service/src/facade.rs` | **new.** The five endpoints, their JSON shapes, the `UploadObjects` gRPC framing, and the `Log`/`GetObjects` collection helpers |
| `crates/forgevcs-service/src/http.rs` | `AppState.config`; `engine_for`/`auth_engine`; `authenticate` and `caller_request` shared by every endpoint; the five routes |
| `crates/forgevcs-service/src/engine.rs` | `Engines::get_or_boot` — create-or-get for `POST /init`, with a second lock check so concurrent initializations converge |
| `crates/forgevcs-service/src/main.rs` | keeps `Config` for `AppState` instead of moving it into `EngineHealth` |
| `crates/forgevcs-service/Cargo.toml` | `scion-object` (decode a changeset for `commits`), `tonic-prost`/`prost` (frame uploads), `futures` (drain streams), `base64`, `hex` |
| `acl.toml` | exact `"main"` update rule — `refs/heads/*` does not match the ref upstream bootstraps |

No vendored file changed in part 3. The one vendored edit remains the `pub mod interceptor`
visibility change from part 2.

### 8.2 Endpoints

| Endpoint | Upstream handlers it calls | Authorization |
| --- | --- | --- |
| `POST /repos/{repo}/init` | `Engine::connect` → `ServerApp::build` (which bootstraps root + `main`) | `write` on `/` under the current policy |
| `GET /repos/{repo}/refs` | `MetaStore::list_refs` (no upstream `ListRefs` RPC) | `read` on `/` |
| `GET /repos/{repo}/commits` | `ObjectService.ResolveRef` → `QueryService.Log` | refs readable; `Log` applies its path/root read checks |
| `POST /repos/{repo}/pull` | `ResolveRef` → `Log` → `ObjectService.GetObjects` | as above; `GetObjects` applies its per-id read check |
| `POST /repos/{repo}/push` | `WriteService.UploadObjects` → `CreateChangeset` → `Land` or `UpdateRef` | upload/create/land policies, inside the handlers |
| `GET /repos/{repo}/changesets/{id}` | `QueryService.GetChangesets` | `GetChangesets` (repository read; filtered delta for unreadable paths) |
| `GET /repos/{repo}/file?changeset=&path=` | `QueryService.ReadPath` (stream) | `ReadPath` (read on the path) |
| `GET /repos/{repo}/tree?changeset=&path=` | `QueryService.ListTree` | `ListTree` (read on the directory) |
| `GET /repos/{repo}/diff?a=&b=&paths=` | `QueryService.Diff` (stream) | `Diff` (repository read plus every path) |
| `GET /repos/{repo}/merge-base?a=&b=` | `QueryService.MergeBase` | `MergeBase` (repository read) |

Ids use Scion's text form (`b1…`, `t1…`, `c1…`, 68 characters). Object content and pulled
bytes are standard base64. `pull` requests upstream's range-read path (`offset = 0`) so the
returned bytes are the decompressed content, not the store's zstd frame. JSON bodies are
capped at 32 MiB — room for base64 of a 16 MiB blob — because axum's 2 MiB default is smaller
than one upstream blob. `commits`/`pull` return `next_page_token` (hex) and accept it back as
`page_token`; an empty token is the first page.

`push` shape:

```json
{
  "objects": [{"id": "b1…", "data": "<base64>"}],
  "changeset": {
    "parents": ["c1…"],
    "delta": [{"path": "hello.txt", "op": 1, "mode": 1, "id": "b1…", "size": 6}],
    "meta": {"author": "Maestro <maestro@example.test>", "message": "add hello"},
    "change_id": "00112233445566778899aabbccddeeff"
  },
  "ref": "main",
  "mode": "land"
}
```

`mode` is `land` (default), `update` (with `expected_version`/`allow_non_fast_forward`), or
`none` (create the changeset but move no ref). A response carries the changeset ids, the
per-object upload status, and the resulting ref/landed tuple. An upload that fails validation
for any object is `400 upload_failed` and no changeset is created.

`pull` / `commits` return commits as `{id, parents, root_tree, change_id, filtered, meta}`;
`pull` adds the requested `objects` with `{id, status, encoding, total_size, final, data}`.
A repository that was not booted in this process is `404 not_found` and names the `init` call;
reads never create a repository.

The W4 read surface closes the V0 work order's §8 acceptance. `changesets/{id}` decodes the
canonical changeset (or returns upstream's filtered view); `file` returns the entry plus base64
bytes; `tree` returns `{name, mode, id, size}` for one directory; `diff` returns the changed
paths with `old`/`new` entries and text hunks (lines carry their ` `/`-`/`+` prefix) and accepts
a comma-separated `paths` scope; `merge-base` returns every best common ancestor. `ReadPath` and
`Diff` stream upstream, so the façade collects each stream before answering.

### 8.3 One auth path, one verifier

Every business endpoint starts with `http::authenticate`: it reads the bearer token through
`interceptor::authenticate`, builds `CallerCtx`, and the handler receives it in the request
extensions. There is no second token parser and no policy check copied from a handler, except
the two operations with no handler to carry one — repository creation (`write` on `/`) and the
ref list (`read` on `/`).

The verifier is the process's **default** engine, not the engine named by the path. That is
deliberate: with no `FORGEVCS_SIGNING_KEY_PATH`, each engine generates its own ephemeral key,
so a token minted by `/auth/token` (which uses the default engine) would not verify on a repo
initialized later. Handlers only read the `CallerCtx` the façade inserts. With a real signing
key configured, every engine loads the same keyring and the choice is immaterial.

### 8.4 Acceptance

```sh
cargo fmt --all --check                                          # clean
cargo clippy -p forgevcs-service --all-targets -- -D warnings    # 0 warnings
cargo test -p forgevcs-service --lib --bins                      # 30 passed
scripts/gate.sh                                                  # 1343 passed / 0 failed
scripts/engine-smoke.sh                                          # exit 0
python3 scripts/w4-acceptance.py --base … --token … --repo …     # 20 checks, exit 0
```

`engine-smoke.sh` runs probes and the auth loop, then invokes `scripts/w4-acceptance.py`
against the live process. The client is deliberately usable against the hosted deployment in
W8; it creates the repo, lands changeset A, modifies the same path to B, and asserts history,
`B.parent == A`, both file versions, the tree, the diff and the merge base. The current run:

```
PASS  repository initialized
PASS  changeset A landed
PASS  changeset B landed
PASS  B.parent == A
PASS  file at A / file at B
PASS  tree lists the path
PASS  diff has both revisions
PASS  merge-base is A
OK  20 checks passed
```

The script exits non-zero if any assertion fails; this run exited 0.

```
init(forgevcs-smoke) HTTP 200  {"created":true,"refs":[{"name":"main","target":"c12597…","version":1}],…}
refs(forgevcs-smoke) HTTP 200  {"refs":[{"name":"main","target":"c12597…","version":1}],…}
push(forgevcs-smoke) HTTP 200  {"changeset":{"id":"c10da9…","root_tree":"t1f699…"},"ref":{"name":"main","target":"c10da9…","version":2},"uploads":[{"id":"b18e4c…","status":"ok"}],…}
push-noauth         HTTP 401  {"code":"unauthenticated","message":"missing bearer token",…}
commits(forgevcs-smoke) HTTP 200  {"commits":[{"id":"c10da9…","meta":{"message":"hello from the facade"},…}],…}
pull(forgevcs-smoke) HTTP 200  {"objects":[{"data":"aGVsbG8K","encoding":"raw","status":"ok","total_size":6}],…}
```

The script exits non-zero if any status, the tip message, or the pulled bytes differ; this run
exited 0.

### 8.5 Findings

1. **The shipped policy could not advance the ref it bootstraps.** `RefName::main()` is the
   literal `main`; the policy granted `"refs/heads/*"`, whose prefix rule does not match it.
   The root bootstrap bypasses policy, so the W4 part 1 smoke never noticed. Every first push
   would have been `PERMISSION_DENIED`. Fixed with an exact `"main"` rule; forced updates still
   require `force`, which nobody has.
2. **`UploadObjects` is the one streaming handler.** Its request is
   `Streaming<UploadItem>`, not a vector, so a REST body cannot be passed through. The façade
   builds the five-byte gRPC frame header and prost payloads and lets tonic decode them; the
   handler, its validation and its ownership records are unchanged. This is wire framing, not
   a second upload implementation.
3. **Ephemeral keys are per engine.** Without a signing key, booting a second repository
   creates a second random keyring. Verification therefore uses one process-wide verifier
   (§8.3) instead of the addressed repo's; production should set
   `FORGEVCS_SIGNING_KEY_PATH`, at which point all engines share a key.
4. **`init` creates a Scion repository, not a `forgevcs_repos` row.** `CrdbMetaStore::connect`
   inserts the `repos` row and `ServerApp::build` bootstraps root + `main`, so `init` is a real
   initialization with no new table. D6's `forgevcs_repos` remains the place for Forge-only
   metadata (owner, visibility, workspace); adding it now would duplicate the source of truth
   before W5 needs it.
5. **`GET /refs` is the one direct store read.** Upstream has `GetRefLog` but no list RPC.
   The facade calls `MetaStore::list_refs` after the same `read` on `/` check `GetRefLog`
   performs. It is the only business read that does not enter a handler.
6. **The read surface is two streaming handlers and one direct decode.** `ReadPath` and `Diff`
   return `BoxStream`; the façade drains them into one JSON response. `changesets/{id}` decodes
   the canonical changeset locally so the response carries parents, root tree and delta rather
   than the encoded blob. `tree` names are UTF-8 text; a repository path that is not valid UTF-8
   is rendered lossily, so a byte-exact client should use `changesets/{id}`/`diff` object ids
   rather than the display name.
7. **A process knew only the repositories it had booted.** Repos initialized on demand lived in
   `Engines` for the process lifetime; the `repos` rows were durable, but nothing enumerated
   them. The hosted W8 run made this visible when Vercel routed a later read to another
   container. The fix keeps the "a typo must not mint a repository" rule: `repo_engine` first
   checks the `repos` row, and only then boots the engine on demand. The first hosted run that
   needed it is recorded in `docs/W8-hosted-smoke.md` §3.

### 8.6 Remaining gaps

- **Repository deletion** has no upstream primitive and no façade endpoint. Deleting a repo
  means deleting every Scion table row for its `repo_id` plus the object keys; that needs a
  reviewed destructive path, not an ad-hoc `DELETE`.
- **Human login**: `FORGEVCS_GITHUB_CLIENT_ID` is still unset, so only service principals can
  mint tokens.
- **The gRPC layers do not wrap the façade.** `ServerApp`'s per-principal rate limiter, load
  shedding, request-id propagation, RPC logging and epoch trailers are tower layers around
  the generated gRPC services; calling handlers in-process skips them. This is the same class
  of gap ADR-0002 records for `/auth/token`, now broader: `push` is an unauthenticated-cost
  endpoint behind a valid token, and REST responses carry no epoch trailers. Until the
  limiter is reused at the handler seam, the platform must provide request-rate limiting and
  the client must not rely on epoch trailers over REST.

