# ADR-0003 — WorkspaceService over Scion's working-copy client

- **Status:** accepted
- **Date:** 2026-10-17
- **Related:** ADR-0001 §5.2 (gRPC stays internal), ADR-0002 D4 (REST façade stays in-process), `docs/W5-workspace-service.md`

## Context

The V0 work order requires exactly one `WorkspaceService` wrapping Scion's existing working-copy
functionality (`create`, `materialize`, `status`, `read`, `write`, `delete`, `diff`, `submit`,
`destroy`). The vendored implementation is `scion-wc`: it owns `.scion/state`, the stat cache,
checkout/goto, commit/land, conflict materialization and the safety rules for paths and symlinks.

`scion-wc` does not accept an in-process backend. `WorkingCopy` is built around
`scion_proto::ScionClient`, whose only transports are HTTPS (TLS required) and Unix sockets.
There is no trait between the two, and rewriting that client boundary would touch the vendored
crate far beyond an edge adapter.

Three options were considered:

1. **Reimplement a workspace as an in-memory overlay** over the REST handlers. Rejected: it
   would duplicate tree building, diff, commit and conflict semantics that already exist and
   are tested upstream — the exact "rewrite rather than adapt" the work order forbids.
2. **Loopback TCP with TLS.** Rejected: it needs certificate material per process and exposes a
   second network listener for no benefit.
3. **A private Unix-socket gRPC surface per engine.** Chosen. ADR-0001 §5.2 already says gRPC
   stays internal/local and is never Vercel-facing; ADR-0002 D4's "no loopback" rule is about
   the REST façade not duplicating authorization, not about a service that genuinely *is* a
   client of Scion's working-copy implementation.

## Decision

- Each `Engine` starts its own `ServerApp::router()` on a `UnixListener` in a fresh
  `tempfile::TempDir`, then connects a `ScionClient` to it. The socket is private to the process
  and disappears with the engine.
- The client authenticates as `service:forgevcs-workspace`. The engine mints that token from its
  own keyring (24 h); the principal is in the `agents` group in `acl.toml` because the working
  copy must read and write paths. **The façade checks the external caller's policy before every
  workspace read, write, delete and submit**, so the privileged internal principal is not a
  bypass of the caller's permissions.
- A workspace is a `tempfile::TempDir` materialization plus a per-workspace lock. The
  `WorkspaceService` opens a fresh `WorkingCopy` for each operation (its `.scion/state` is on
  disk), performs the operation, and closes it. There is no long-lived in-memory WC handle.
- The registry is per engine and in memory. Workspace IDs are UUIDs. A restart empties the
  registry; IDs answer 404 and a client creates a new workspace from a ref or changeset.
- `submit` commits with `WorkingCopy::commit_with_report`, then either lands on the requested
  ref or stops at the unlanded changeset (`mode: "none"`). `destroy` removes the registry entry
  and its temporary directory.

## Consequences

- **Canonical state is still Neon.** A workspace is a cache; deleting it, losing the container,
  or scaling to zero loses only the materialization. History is in Postgres + Object Storage.
- **No workspace survives a restart.** V0 has no workspace table and no container affinity. A
  workspace ID is valid only in the process that created it; after a restart the client must
  create another. This is deliberate, not a silent failure: the 404 says so.
- **One extra Unix listener per engine.** It is not reachable from outside the process; no
  Vercel route, no port, no TLS. The socket directory is a `TempDir` and is removed with the
  engine (a SIGKILL can leave a stale `/tmp` directory; the OS temp cleaner owns that).
- **The internal principal is a real policy grant.** Anyone who can mint tokens with the
  engine's signing key can act as it; that is the same trust boundary as the service itself.
  With `FORGEVCS_SIGNING_KEY_PATH` set, the token is stable across restarts (though the
  workspace IDs are not).
- **Token lifetime is 24 h.** A process that lives longer than that cannot open new workspaces
  until it restarts. The Vercel container's 5-minute idle scale-down makes that theoretical for
  M0; a long-lived deployment needs token refresh or a non-expiring internal identity.
- **No rebase/conflict endpoints yet.** `scion-wc` has them; the façade exposes only the V0
  operations. A paused rebase is visible as a pending operation in `.scion/state` but not yet
  over REST.

## Alternatives rejected

See Context. The short version: the only implementation that keeps Scion's working-copy
semantics without a second copy of them is the real `scion-wc`; the only transport it accepts
without TLS is a Unix socket.
