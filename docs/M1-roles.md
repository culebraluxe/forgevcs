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
| Create a repository | **no (403)** | yes | façade `init` gate on `admin.refs` (this commit) |
| Delete a repository | **no** | **yes, when it exists** | no endpoint yet; it must take `admin.refs` |
| Advance `main` / `land` / `merge-land` | **no (403)** | yes | `[refs]` `main = group:devops` in the strict policy |

`admin.refs` is the DevOps capability today because it is the closest existing upstream
permission; a dedicated `admin.repos` permit is an engine-version change and is not required to
ship the role split.

## What is enforced in code vs policy

- **Code (this commit):** `POST /repos/{repo}/init` refuses a principal without `admin.refs`
  when it would *create* a repository (`created == true`). Calling `init` on an existing
  repository stays available so any principal can bind its engine on demand.
- **Policy:** `[refs] main = group:devops` and the per-agent lane rules are the merge-advance and
  lane-isolation enforcement. The collaboration pilot (`scripts/pilot/acl.toml`) deliberately
  keeps `main` writable by the trio so it can measure merge/land mechanics; the strict matrix
  lives in `scripts/pilot/acl-roles.toml`.
- **Absent:** repository deletion. When it is built it must check the same DevOps seat, and the
  same gate belongs on any future backup/restore or retention operation.

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
