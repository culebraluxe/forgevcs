# `forgevcs` — the human CLI

A small command-line client for the ForgeVCS REST façade. It is a **thin wrapper**: every
operation is one HTTP call to a route the service already implements
(`crates/forgevcs-service/src/http.rs`), with a locally stored bearer token. There is **no Git
wire protocol**, no browser login, no deploy/publish path, and no second authorization rule —
the service's policy and repository ACLs decide everything.

Status: v1, the human path from `docs/WORK-ORDER-HUMAN-CLI.md` §2 (the work order's 12 commands
plus `ref create`, and `auth logout`). The principal is `culebraluxe <culebraluxe@gmail.com>`;
the token-mint path is the service's `POST /auth/human-token` (a configured human login + locally
supplied secret), so the same `github:<login>` identity policy already grants is used without a
browser.

## Build and run

```sh
cargo build -p forgevcs-cli            # binary: target/debug/forgevcs
cargo run -q -p forgevcs-cli -- --help
cargo test -p forgevcs-cli             # unit + mock-server tests, no external network
```

## Configuration

Global options go **before** the command:

```sh
forgevcs [--base-url URL] [--config-dir PATH] [--token-file PATH] <command> [options]
```

| Setting | Flag | Environment | `config.toml` | Default |
|---|---|---|---|---|
| Service base URL | `--base-url` | `FORGEVCS_BASE_URL` | `base_url` | `http://127.0.0.1:3000` |
| Config/token directory | `--config-dir` | `FORGEVCS_CONFIG_DIR` | — | `$XDG_CONFIG_HOME/forgevcs` or `~/.config/forgevcs` |
| Token file | `--token-file` | `FORGEVCS_TOKEN_FILE` | `token_file` | `<config-dir>/token` |
| Bearer token (no file) | — | `FORGEVCS_TOKEN` | — | unset |

Precedence is flag > environment > `config.toml` > default. `config.toml` is optional and
strict (unknown keys are an error, so a typo cannot silently fall back to a wrong target):

```toml
base_url = "https://culebraluxe-forgevcs.vercel.app"
# token_file = "token"   # relative paths are resolved inside the config directory
```

The production URL must be set explicitly; the default points at a local service.

## Authentication

### Mint a token (no browser)

The service is configured with a human principal whose secret digest lives in
`FORGEVCS_HUMAN_PRINCIPALS`; the CLI supplies the secret once and receives a bearer token:

```sh
# secret via a 0600 file (recommended: never on the command line or in shell history)
forgevcs auth mint --login culebraluxe --secret-file ~/.config/forgevcs/human-secret

# or via the environment
FORGEVCS_HUMAN_SECRET=… forgevcs auth mint --login culebraluxe

# or stdin (prompt is visible; fine for a throwaway secret, not a shared terminal)
forgevcs auth mint --login culebraluxe
```

The token is written to `<config-dir>/token` with mode **0600** and never printed (unless you
explicitly pass `--show-token`). A non-secret `session.json` (principal, email, display, expiry,
mode 0600) is stored next to it so `merge`/`merge-land` can default the commit author to
`Display <email>`. `forgevcs auth logout` removes both files.

If a token file is group/world-readable, the CLI tightens it to 0600 on read instead of refusing;
a secret file passed to `auth mint --secret-file` must already be private (0600).

For a short-lived shell or CI job, `FORGEVCS_TOKEN` supplies the bearer token directly and wins
over the stored file, so no token needs to be written to disk.

### Inspect the session

```sh
forgevcs auth status
# { "base_url": …, "config_dir": …, "token_file": …, "token_present": true,
#   "token_source": "file", "session": { "principal": "github:culebraluxe", … },
#   "whoami": { … } }
```

`auth status` is informative rather than fatal: a stale/revoked token is reported as
`whoami_error` next to the local state. `token_present` covers both credential sources and
`token_source` is `"env"` for `FORGEVCS_TOKEN` or `"file"` for the stored token; neither value is
the token itself, so it is safe to share in a bug report.

## Commands

Every command prints the service's JSON response pretty-printed on stdout. The CLI adds no
semantics of its own beyond assembling the documented request body.

| Command | Façade route |
|---|---|
| `auth status` | `GET /auth/whoami` (when a token exists) |
| `auth mint --login <login> [--secret-file PATH] [--show-token]` | `POST /auth/human-token` |
| `auth logout` | local only |
| `health` | `GET /health`, `GET /version`, `GET /ready` |
| `init <repo>` | `POST /repos/{repo}/init` |
| `refs <repo>` | `GET /repos/{repo}/refs` |
| `ref create <repo> --name <ref> --target ID [--reason TEXT]` | `POST /repos/{repo}/refs` |
| `log <repo> [--ref main] [--limit N] [--path P] [--first-parent-only] [--page-token HEX]` | `GET /repos/{repo}/commits` |
| `pull <repo> [--ref main] [--limit N] [--path P] [--objects ID[,ID…]]` | `POST /repos/{repo}/pull` |
| `push <repo> --changeset FILE [--ref main] [--mode land\|update\|none]` | `POST /repos/{repo}/push` |
| `merge <repo> --a ID --b ID --change-id HEX32 [--author …] [--message …]` | `POST /repos/{repo}/merge` |
| `rebase <repo> --changeset ID --onto ID` | `POST /repos/{repo}/rebase` |
| `merge-land <repo> --head ID [--ref main] [--author …] [--message …] [--change-id HEX32]` | `POST /repos/{repo}/merge-land` |
| `workspace create <repo> [--ref R \| --changeset ID] [--sparse a,b]` | `POST /repos/{repo}/workspaces` |
| `workspace status <repo> <workspace-id>` | `GET /repos/{repo}/workspaces/{id}` |
| `workspace destroy <repo> <workspace-id>` | `DELETE /repos/{repo}/workspaces/{id}` |

`push` takes a full façade `PushBody` JSON file (objects are content-addressed: the blob id must
be the BLAKE3 of the base64-decoded `data`; see
[`demo/push-body.json`](demo/push-body.json) for a working one-file example). `--ref`/`--mode`
override the file's values. `merge` requires a caller-supplied 32-hex `--change-id`;
`merge-land` lets the service derive one when omitted.

### Lane refs and `push --mode update`

`init` bootstraps `main`; it does not create lane refs. `ref create` provisions one at an
existing changeset and requires the `admin.refs` seat:

```sh
# by an admin.refs principal:
forgevcs ref create REPO --name lane/a --target <changeset-id> --reason "cut lane/a"
forgevcs refs REPO        # the new ref and its version
```

`--mode update` is a **version-checked update of an existing ref**: the changeset file must carry
`expected_version` (take it from `forgevcs refs`) and the ref must already exist.

- ref missing → the façade answers `404 not_found` (`forgevcs: HTTP 404: not_found: …`,
  non-zero exit); it is not created implicitly by `push`, use `ref create` first;
- stale `expected_version` → `409 conflict`, with the current version/target in the error
  `details`;
- `--mode land` (default) needs no lane ref but only lands on the repository trunk (`main`);
- `--mode none` only creates the changeset and updates no ref.

### Errors and exit status

A non-2xx answer or a local failure is printed to stderr as
`forgevcs: HTTP <status>: <facade code>: <message>` (for example
`HTTP 403: permission_denied: push denied: …`) and the process exits non-zero. `health` is the
one partially-tolerant command: `/ready` may answer 503 while the service starts, and the
answer is reported in the JSON instead of failing the probe. There is currently a single
failure exit code (`1`); the façade's `code` is preserved in the message but not mapped to a
distinct code yet (see "Not in v1").

## Demo

`demo/demo.sh` runs the acceptance flow of `docs/WORK-ORDER-HUMAN-CLI.md` §3 against a service
you point it at:

```sh
cargo build -p forgevcs-cli
export PATH="$PWD/target/debug:$PATH"
forgevcs auth mint --login culebraluxe --secret-file ~/.config/forgevcs/human-secret
FORGEVCS_BASE_URL=https://… ./crates/forgevcs-cli/demo/demo.sh
```

It performs `health → auth status → init → refs → log → push (one file) → ref create (lane ref at
the pushed changeset) → refs/log → pull the object → workspace create/status/destroy`.
`merge`/`rebase`/`merge-land` need two divergent changesets; pass `DEMO_A=<id> DEMO_B=<id>` to
include them, or use the printed invocations.

## Tests

`cargo test -p forgevcs-cli` — everything runs against in-process axum mocks; no database,
object store or external network.

- `tests/rest.rs` — the human acceptance walk-through (mint → init → push → refs/log → merge →
  pull) and every command hitting its façade route.
- `tests/human_cli_contract.rs` — the surrounding contract: argv refusals, config precedence,
  0600 token/session storage and loose-file tightening, mint refusal, request shapes for
  `ref create`/`log`/`pull --objects`/`push`/`merge`/`rebase`/`merge-land`/workspace, `health`'s
  three probes, refusal code preservation, and the no-token mint hint for every authenticated
  command.

## Work-order coverage

`docs/WORK-ORDER-HUMAN-CLI.md` §2 maps onto the commands one-to-one (plus `ref create`, added so
lane refs can be provisioned without an out-of-band REST call); §3's acceptance flow is the
`tests/rest.rs` walk-through (mint → init → push → refs/log → merge → pull).

| Work order | Command(s) | Façade route(s) | Covered by |
|---|---|---|---|
| `auth` (mint/status) | `auth mint`, `auth status`, `auth logout` | `/auth/human-token`, `/auth/whoami` | `tests/rest.rs` acceptance + auth-status tests; `tests/human_cli_contract.rs` mint success/refusal, status, logout |
| `init` | `init` | `/repos/{repo}/init` | both test files |
| `refs` | `refs` | `/repos/{repo}/refs` | both |
| lane provisioning | `ref create` | `/repos/{repo}/refs` | contract test checks `name`/`target`/`reason`, bearer, and the `admin.refs` 403 |
| `log` | `log` | `/repos/{repo}/commits` | rest acceptance; contract test checks `--ref/--limit/--path/--first-parent-only/--page-token` |
| `pull` | `pull` | `/repos/{repo}/pull` | rest acceptance; contract test checks `--objects` |
| `push` | `push` | `/repos/{repo}/push` | rest acceptance + `--ref`/`--mode` override test; contract test checks the body, overrides and a 403 refusal |
| `merge` | `merge` | `/repos/{repo}/merge` | rest acceptance; contract test checks `a`/`b`/`change_id`/`meta` |
| `rebase` | `rebase` | `/repos/{repo}/rebase` | contract test checks `changeset`/`onto` |
| `merge-land` | `merge-land` | `/repos/{repo}/merge-land` | contract test checks `ref`/`head`/`message` |
| `workspace create/status/destroy` | `workspace …` | `/repos/{repo}/workspaces…` | contract test checks all three methods/bodies |
| `health` | `health` | `/health`, `/version`, `/ready` | both test files |
| non-zero exit + typed error | — | — | 401/403 produce a non-zero exit and keep the façade `code`/`message`; see "Not in v1" for the exit-code taxonomy |

## Not in v1 (known limits)

- **No Git wire protocol** (`git clone/push` is explicitly out of scope); the REST façade is the
  interface.
- **No `push --dir`/working-tree snapshot.** `push` needs a hand-written `PushBody` JSON whose
  blob/tree ids are content-addressed. A future convenience could compute BLAKE3 blob ids and
  canonical tree objects with the vendored `scion-core`/`scion-object` crates (the same code the
  service uses) and diff against the ref tip — deliberately not added in v1 to keep the wrapper
  small.
- **No `pull --out` materialization.** `pull` returns JSON (object bytes are base64), it does not
  write a working tree.
- **No typed exit-code taxonomy.** One non-zero code (`1`); the façade error code is in the
  message.
- **Lane refs still need `admin.refs`.** `ref create` wraps the route, but the authorization
  decision is the service policy's; a worker without the DevOps seat gets `403 permission_denied`.
- **No retries**; requests have a 30 s timeout.
- **No production enablement**: pointing the CLI at production and minting a token is an explicit
  principal action, not a default.

## Security notes

- The bearer token lives in a 0600 file; the config directory is created/kept 0700.
- The token is never printed by default, never passed on the command line, and never stored in
  the repository. The human secret is only read from a private file, the environment, or stdin.
- The CLI refuses nothing itself: all authorization is the service's policy + repository ACLs, so
  a refusal (`403 permission_denied`) is the service's decision, reported verbatim.
