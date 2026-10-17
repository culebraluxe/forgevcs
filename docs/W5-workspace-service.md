# W5 — WorkspaceService

**Status:** complete 2026-10-17. One `WorkspaceService` wraps the vendored `scion-wc`
implementation over a private Unix-socket gRPC surface. A workspace is a disposable
materialization; its state and dirty-file overlay are persisted in Neon, so any container can
restore it after scale-to-zero. The 17-check `scripts/w5-acceptance.py` client creates, writes,
diffs, submits, destroys, and recreates a workspace to prove history survives disposal.

## 1. What this part solves

The work order's W5 requires one service abstraction with
`create/materialize/status/read/write/delete/diff/submit/destroy`, and requires that a workspace
disappearing cannot lose source history:

```text
workspace disappears
        ↓
source history remains intact
```

Vercel Functions scale to zero after five idle minutes, so local disk cannot be the authority.
The workspace layer must therefore be a cache in front of Neon, not a second repository.

## 2. Architecture

```text
REST /repos/{repo}/workspaces/...
        │
        ▼
WorkspaceService (per Engine)
        │
        ├── registry: workspace id -> TempDir + lock
        ├── forgevcs_workspaces: id, repo, WcState JSON
        ├── forgevcs_workspace_files: dirty path, mode, bytes
        │
        └── WorkingCopy::open/checkout/... (scion-wc)
                    │
                    ▼
            private Unix-socket gRPC
                    │
                    ▼
            Scion ServerApp (same Engine)
                    │
                    ▼
            Neon Postgres + Neon Object Storage
```

The internal gRPC surface is the decision in
[ADR-0003](adr/0003-workspace-service.md): `scion-wc` is a `ScionClient` consumer, and the only
transport it accepts without TLS is a Unix socket. The socket is created in a `tempfile::TempDir`
per engine, is never exposed by Vercel, and disappears with the engine. REST remains the only
external API (ADR-0002 D4 / ADR-0001 §5.2).

The internal client authenticates as `service:forgevcs-workspace`, a principal the engine mints
from its own keyring. The external caller's policy is checked **before** the working copy is
asked to touch a path, so the privileged internal principal is not a privilege escalation.

## 3. Endpoints

| Endpoint | Operation |
| --- | --- |
| `POST /repos/{repo}/workspaces` | `create`: materialize `{ref}` or `{changeset}` (default `main`), optional `sparse` |
| `GET /repos/{repo}/workspaces/{id}` | `status`: modified/added/deleted/mode-changed/redacted-dirty paths, base and tracking ref |
| `POST /repos/{repo}/workspaces/{id}/materialize` | `materialize`: `goto` another ref/changeset (`force` for dirty paths) |
| `GET /repos/{repo}/workspaces/{id}/file?path=` | `read`: file entry and base64 bytes |
| `PUT /repos/{repo}/workspaces/{id}/file?path=` | `write`: base64 bytes, mode `1` file / `2` executable / `3` symlink target |
| `DELETE /repos/{repo}/workspaces/{id}/file?path=` | `delete` |
| `GET /repos/{repo}/workspaces/{id}/diff?context=3` | `diff` with hunks, filtered to readable paths |
| `POST /repos/{repo}/workspaces/{id}/submit` | `commit` then `land` (`mode:"land"`, default) or leave unlanded (`mode:"none"`) |
| `DELETE /repos/{repo}/workspaces/{id}` | `destroy` the materialization |

Workspace IDs are UUIDs persisted in `forgevcs_workspaces`. The temporary directory is only a
cache: when a request reaches an instance that does not have the workspace, `WorkspaceService`
reads the persisted `WcState`, checks out its base changeset, re-applies the dirty-file overlay
from `forgevcs_workspace_files`, and continues. A restart or a different Vercel instance therefore
does not turn the ID into a 404. The overlay is updated on every write/delete; `submit` and a
clean `materialize` clear it. `destroy` deletes the row and cascades the overlay.

The current limits of that restore are explicit: a workspace moved while a rebase is paused is
not replayed (the pending operation is in `WcState`, but the resolved/unresolved conflict files
are not part of the overlay), and two instances doing concurrent writes to one workspace are not
serialized by a distributed lock. Both are W5 gaps, not silent promises.

`submit` commits with `WorkingCopy::commit_with_report` and then lands through
`WorkingCopy::land`, so the upload, change-set construction and land rules are Scion's. The
response carries the changeset id, root tree, change id, parents, delta paths, upload count, and
the ref/landed report.

## 4. Authorization

- `create`/`materialize`/`status`/`diff`: `read` on `/` (status and diff additionally filter each
  path through the policy, so a caller never sees an unreadable path in a listing).
- `read`: `read` on the path.
- `write`/`delete`: `write` on the path.
- `submit`: `write` on every changed path, plus `update` on the target ref for `land`.
- `destroy`: `write` on `/`.

The external checks run in the façade; `scion-wc` runs as the internal principal. This is the
only way to keep Scion's working-copy semantics without teaching the vendored crate about the
external caller.

## 5. Acceptance

```sh
cargo fmt --all --check
cargo clippy -p forgevcs-service --all-targets -- -D warnings
cargo test -p forgevcs-service --lib --bins
scripts/gate.sh
scripts/engine-smoke.sh
python3 scripts/w5-acceptance.py --base … --token … --repo forgevcs-workspaces
```

The client run against Neon:

```
PASS  workspace created at main
PASS  root sparse renders as /
PASS  new workspace is clean
PASS  file written
PASS  status sees the added file
PASS  diff sees the added file
PASS  diff carries the content
PASS  workspace read returns the bytes
PASS  submit landed
PASS  changeset is canonical
PASS  changeset delta has the path
PASS  read through W4 returns the bytes
PASS  destroyed workspace is 404
PASS  history survives workspace disposal
PASS  materialize moved to the old changeset
PASS  materialize moved back to main
PASS  file returns after moving back
OK  17 checks passed
```

The script exits non-zero on any failure; this run exited 0.

Persistence is additionally proven across processes: create a workspace and write a file in
process A, stop A, start B, then `GET status`, `GET file` and `POST submit` on the same workspace
ID in B. The observed responses were 200 with the dirty file present and the commit landed. The
hosted W8 smoke exercises the same property across Vercel instances.

One full gate run on this tree failed the upstream test
`object_upload::upload_batches_preserve_order_and_reduce_metadata_writes` (265 vs 263 metadata
writes). The same test passed in isolation immediately afterwards, and the next full gate
passed 1343/0. No upstream file changed; the failure is recorded here rather than hidden.

## 6. Findings

1. **`scion-wc` has no in-process client seam.** `WorkingCopy` is built on
   `scion_proto::ScionClient`, which accepts HTTPS (TLS mandatory) or a Unix socket. Reusing the
   real implementation therefore needed the private Unix-socket gRPC surface; reimplementing
   working-copy semantics was rejected (ADR-0003).
2. **The external caller's policy had to move up.** The internal principal is privileged because
   `scion-wc` has to read and write files. Every façade operation checks the external caller
   first; status and diff also filter paths so an unreadable path is not disclosed through a
   listing or a hunk.
3. **A workspace is state on disk, not a handle.** `WorkingCopy::open` reloads `.scion/state`, so
   each request opens a fresh handle under a per-workspace lock. A long-lived in-memory handle
   would have to be `Send + Sync` across awaits and would keep the internal state stale if two
   requests interleaved.
4. **Workspace IDs now survive restarts and instances.** The first hosted W5 run failed when
   Vercel routed a later request to another container; the fix persists `WcState` and the
   dirty-file overlay in Neon and restores the temp directory on demand. The 404-for-a-lost-
   workspace semantics was not good enough once W8 ran on the real platform.
5. **The internal token lives 24 hours.** A process older than that cannot open new workspaces
   until restart. The Vercel container's 5-minute idle scale-down makes this theoretical for M0;
   a long-lived deployment needs a refresh path or a non-expiring internal identity.

## 7. Remaining gaps

- **Concurrent writes to one workspace from two instances** are not serialized across the
  database; the per-workspace mutex is process-local. V0 assumes one active editor per workspace.
- **Paused rebases are not restored exactly**: the pending operation is persisted in `WcState`,
  but conflict-file materialization/resolution state is not part of the overlay.
- **Rebase/conflict operations** (`rebase`, `resolve`, `abort`) are in `scion-wc` but not exposed
  by the V0 REST surface. A paused rebase is visible in `.scion/state` but has no endpoint.
- **Workspace listing and quotas.** There is no `GET /workspaces` and no cap; a client that
  creates workspaces without destroying them keeps DB rows and temp directories until destroyed.
- **TempDir cleanup on SIGKILL.** A killed process can leave a `/tmp` directory; the OS temp
  cleaner owns it. A normal `destroy` or process drop removes it.
