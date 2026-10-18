# DevOps and worker authority

**Date:** 2026-10-17 · **Tree:** `cc823f3` + the DevOps gate and role pilot in this commit

The authority model is two roles, not per-agent special cases:

- **Worker** — the ordinary agent. Check-in (push a changeset to its own lane ref), check-out
  (workspace/materialize), read, diff, merge-base. It may open a merge changeset but it may not
  advance `main`.
- **DevOps** — one seat. Creates (and, when implemented, deletes) repositories, provisions refs,
  advances `main`, and runs the trunk integration (`land`, `merge-land`). It does not replace the
  workers: it is the only principal with the lifecycle and publication permissions.

The model maps to the existing Culebraluxe role table (`DEVOPS` owns release/publish authority;
Smith/Scout/Assay are workers) and to Scion's own permissions:

| Operation | Worker | DevOps | Where it is enforced |
| --- | --- | --- | --- |
| Read, diff, merge-base, list refs | yes | yes | path `read` on `/` |
| Workspace create / materialize / read / write | yes | yes | path `read` + per-path `write` |
| Push to its own lane ref | yes | yes | `[refs]` rule for `agent/<name>` |
| Push to another agent's lane ref | no (403) | no | `[refs]` rule scoped per agent |
| Create a ref | no (403) | yes | upstream `CreateRef` requires `admin.refs` |
| Create a repository | **no (403)** | yes | façade `init` gate on `admin.repos` + no gRPC path creates repos |
| Delete a repository | **no** | **yes, when it exists** | no endpoint yet; it must take `admin.repos` |
| Advance `main` / `land` / `merge-land` | **no (403)** | yes | façade `admin.trunk` gate **and** `[refs] main = group:devops`, so gRPC cannot bypass it |

Three entitlements make the seat explicit in the engine policy: `admin.repos` (create/delete
repositories), `admin.refs` (create/delete refs) and `admin.trunk` (advance the trunk ref). They
are `[admin]` grants, so a policy that does not name the DevOps principal denies by default. The
façade checks `admin.repos` and `admin.trunk` at the request that would act; the gRPC surface
cannot be used to bypass trunk advance because the `main` ref rule also names the DevOps group.

## What is enforced in code vs policy

- **Code:** `POST /repos/{repo}/init` refuses a principal without `admin.repos` when it would
  *create* a repository. `push` with `mode:"land"|"update"`, `land`, `merge-land` and workspace
  `submit` refuse a principal without `admin.trunk` when the target ref is `main`. Calling `init`
  on an existing repository stays available so any principal can bind its engine on demand.
- **Policy:** `[refs] main = group:devops` is a second, independent deny for trunk advance (a
  direct gRPC `UpdateRef` cannot bypass the façade gate), and per-agent lane rules isolate the
  workers. The collaboration pilot (`scripts/pilot/acl.toml`) deliberately grants `admin.trunk`
  to the trio so it can measure merge/land mechanics; the strict matrix lives in
  `scripts/pilot/acl-roles.toml`.
- **Absent:** repository deletion. When it is built it must check `admin.repos`, and the same gate
  belongs on any future backup/restore or retention operation.

## Conflict escalation queue (next)

An agent that cannot resolve a conflict should not guess. The intended loop is: worker lands →
`409` with the report → `conflict-context` for the three sides → the worker resolves if it can;
if it cannot, it **escalates** and moves on. Escalation is a work item for the DevOps seat that
the orchestrator launches (Cline/DeepSeek as DevOps).

Planned shape (not implemented yet): a ForgeVCS-owned `forgevcs_conflict_escalations` table
(repo, ref, head, onto, conflict report, note, status, created/claimed/resolved timestamps,
resolution), with

- `POST /repos/{repo}/conflicts/escalate` — any worker; returns the escalation id;
- `GET /repos/{repo}/conflicts?status=open` — DevOps (`admin.trunk`);
- `POST /repos/{repo}/conflicts/{id}/claim` and `/resolve` — DevOps; `resolve` records the
  landed changeset and clears the item.

That gives the orchestrator one poll-able list and keeps unresolved merges out of a chat log.

## Prove it

```sh
scripts/role-pilot.sh      # strict policy, worker + devops, port 3997
```

Measured 2026-10-17: the role pilot passes **13/13**, and the collaboration pilot passes
**41/41** with every repository created by the DevOps seat. The role pilot checks: the DevOps seat creates a repository; a worker's create attempt is 403; a
worker cannot create a ref (403); the worker lands a changeset on its own lane; the worker's push
to `main` is 403; the DevOps seat lands that head on `main` and the worker's file is there; the
worker can read refs, compute a merge base and create a workspace. Deletion is recorded as absent.

## Follow-ups

1. Repository deletion endpoint gated on `admin.refs`, then the role pilot proves the deny and
   the allow.
2. Unify the collaboration pilot policy to the strict matrix once the merge work is stable (its
   permissive `main` rule exists only to test CAS/rebase mechanics).
3. If the model asks for a third tier (e.g. reviewer), add it as policy groups first, not as a
   new code path.
