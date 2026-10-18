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
| List repositories (`GET /repos`) | **no (403)** | yes | `admin.repos` |
| Object GC (`POST /admin/gc/objects`) | **no (403)** | yes | `admin.repos`; dry run unless `apply` |
| Delete a repository (`DELETE /repos/{repo}`) | **no (403)** | yes | façade gate on `admin.repos`; the body must repeat the name; metadata rows in one transaction, shared blobs retained |
| Advance `main` / `land` / `merge-land` | **no (403)** | yes | façade `admin.trunk` gate **and** `[refs] main = group:devops`, so gRPC cannot bypass it |

Three entitlements make the seat explicit in the engine policy: `admin.repos` (create/delete
repositories), `admin.refs` (create/delete refs) and `admin.trunk` (advance the trunk ref). They
are `[admin]` grants, so a policy that does not name the DevOps principal denies by default. The
façade checks `admin.repos` and `admin.trunk` at the request that would act; the gRPC surface
cannot be used to bypass trunk advance because the `main` ref rule also names the DevOps group.

## What is enforced in code vs policy

- **Code:** `POST /repos/{repo}/init` refuses a principal without `admin.repos` when it would
  *create* a repository. `DELETE /repos/{repo}` refuses the same seat and requires the body to
  repeat the repository name; it deletes every `repo_id`-keyed Scion table row plus the repo's
  workspace and queue rows in one transaction, then drops the in-process engine. `push` with
  `mode:"land"|"update"`, `land`, `merge-land` and workspace `submit` refuse a principal
  without `admin.trunk` when the target ref is `main`. Calling `init` on an existing repository
  stays available so any principal can bind its engine on demand.
- **Policy:** `[refs] main = group:devops` is a second, independent deny for trunk advance (a
  direct gRPC `UpdateRef` cannot bypass the façade gate), and per-agent lane rules isolate the
  workers. The collaboration pilot (`scripts/pilot/acl.toml`) deliberately grants `admin.trunk`
  to the trio so it can measure merge/land mechanics; the strict matrix lives in
  `scripts/pilot/acl-roles.toml`.
- **Objects are not deleted with a repository**, but they are no longer orphans forever:
  Scion keys are repository-scoped (`/r/<repo_id>/…`), so `POST /admin/gc/objects` deletes keys
  whose repo_id is gone from `repos` and nothing else. It is dry-run by default, malformed keys
  are reported and never deleted, and it is gated to `admin.repos`
  (`scripts/gc-orphan-blobs.sh`). A backup/restore drill is still absent and is the gate for
  calling deletion production-ready.

## Conflict escalation queue (implemented)

An agent that cannot resolve a conflict should not guess. The loop is: worker lands → `409` with
the report → `conflict-context` for the three sides → the worker resolves if it can; if it cannot,
it **escalates** and moves on. Escalation is a work item for the DevOps seat that the orchestrator
launches (Cline/DeepSeek as DevOps).

Implemented as a ForgeVCS-owned `forgevcs_conflict_escalations` table
(`crates/forgevcs-service/src/escalations.rs`, `migrations/0003_…sql`, created at boot like the
workspace/authz tables):

| Route | Caller | What it does |
| --- | --- | --- |
| `POST /repos/{repo}/conflicts/escalate` | any worker | Files `{ref, head, onto?, base?, note?, report?}`; returns the escalation row |
| `GET /repos/{repo}/conflicts?status=open\|claimed\|resolved\|all` | DevOps (`admin.trunk`) | The poll-able work list; `all`/`resolved` is how the data stays visible |
| `POST /repos/{repo}/conflicts/{id}/claim` | DevOps | Moves an open item to `claimed`, records the claimer |
| `POST /repos/{repo}/conflicts/{id}/resolve` | DevOps | Closes it with a resolution note |
| `DELETE /repos/{repo}/conflicts/{id}` | DevOps | Clears one item |
| `DELETE /repos/{repo}/conflicts?status=resolved\|all` | DevOps | Bulk-clears queue history over time |

The queue stores pointers and the conflict report, never content: the DevOps agent re-derives the
three sides with `POST /repos/{repo}/conflict-context` before resolving and landing. A conflicting
merge is claimable once; resolving or clearing an already-closed item is a `404`/`409`.

Orchestrator loop: `GET conflicts?status=open` → for each item launch a DevOps agent → agent
`claim` → reads context → resolves (land or re-create the changeset) → `resolve` with the note.
The item stays queryable as history until the DevOps seat clears it.

## Prove it

```sh
scripts/role-pilot.sh              # strict policy, worker + devops, port 3997
scripts/cleanup-pilot-repos.sh     # dry run: list burn-in repos the DevOps seat may delete

Both pilots delete the repositories they create in teardown (pass or fail), and the hosted W8
smoke does the same for its two repositories unless `KEEP_SMOKE_REPOS=1`; tests no longer leave a
growing mess. `cleanup-pilot-repos.sh --apply` is for older leftovers.
```

Measured 2026-10-17/18: the role pilot passes **31/31**. It proves the DevOps seat creates a
repository, a worker's create/delete/list is 403, a worker cannot create a ref, the worker lands
on its own lane, a worker's trunk advance is 403, DevOps lands the worker's head on `main` and
the file is there, the worker can read refs, compute a merge base and create a workspace, the
queue flow runs end to end with a real `409` (worker escalates; workers cannot list or claim;
DevOps lists, claims and resolves; the open list clears), resolved items stay visible as data,
DevOps clears them, and the DevOps seat deletes a repository (the worker's attempt is 403, the
repository leaves the control plane, and a second delete is 404), and object GC is a DevOps-only
dry run that deletes nothing. The collaboration pilot passes **41/41** with every repository created
by the DevOps seat. The first real cleanup on 2026-10-17 deleted **72 burn-in repositories (5,419
metadata rows)**; the first GC sweep (2026-10-18) then deleted **12 orphan object keys and left 0**
(72 unmatched keys were W3 acceptance fixture files under `forgevcs-ci/w3-*`, not engine objects,
and were reported and kept).

## Follow-ups

1. Backup/restore drill — GC now reclaims blobs of deleted repositories, but a tested
   backup/restore is still the gate for calling deletion production-grade.
2. Unify the collaboration pilot policy to the strict matrix once the merge work is stable (its
   permissive `main` rule exists only to test CAS/rebase mechanics).
3. If the model asks for a third tier (e.g. reviewer), add it as policy groups first, not as a
   new code path.
