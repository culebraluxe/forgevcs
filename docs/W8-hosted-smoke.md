# W8 — Hosted source-control smoke (M0)

**Status:** complete 2026-10-17. The hosted Vercel service passes the W4 and W5 acceptance
clients against Neon Postgres + Neon Object Storage, with no Git in the source-control
operation. After the multi-instance fixes, two consecutive runs passed: W4 20/20 and W5 17/17
each.

- **Host:** https://culebraluxe-forgevcs.vercel.app
- **Script:** `scripts/w8-hosted-smoke.sh`
- **Result:** `W8_HOSTED=PASS`; W4 20/20, W5 17/17

## 1. What the smoke proves

The script reaches the protected deployment through the project's automation-bypass secret,
mints a real Scion token from the deployed `/auth/token`, and runs the same clients that run
locally:

```text
/health   200
/ready    200     # Neon Postgres + Neon Object Storage from the Vercel container
/version  200
token     minted (242 chars)
W4        init → A → B → B.parent == A → history → file A/B → tree → diff → merge-base
          → push without token 401
W5        create workspace → write → status → diff → read → submit/land
          → destroy → recreate → history survives → materialize away/back
W8_HOSTED=PASS
```

A second consecutive run after the per-request restore fix also passed W4 20/20 and W5 17/17,
so the result is not a single warm-container coincidence.

The work order's M0 shape is demonstrated:

```text
Mac / test client
       │
       ▼
ForgeVCS on Vercel
       │
       ├── changeset
       ├── history
       ├── diff
       └── workspace
       │
       ▼
Neon
├── Postgres
└── Object Storage
```

No `git` command participates in those operations. Git still hosts the ForgeVCS source.

## 2. Running it

```sh
scripts/w8-hosted-smoke.sh
FORGEVCS_HOSTED_URL=https://… scripts/w8-hosted-smoke.sh
```

Inputs kept outside the repository:

- `/Users/Shared/dev/.forgevcs-vercel-maestro.secret` — the deployment's `service:maestro`
  secret (mode 600).
- The automation-bypass secret is read from `vercel project protection --json` at run time.

The script creates unique repositories (`forgevcs-w4-<stamp>`, `forgevcs-w5-<stamp>`) and does
not delete them: V0 has no repository-deletion endpoint. Repeated runs therefore add repository
rows and object keys to the branch; the names are unique so the history is not ambiguous.

## 3. Findings

1. **Deployment Protection needs a deliberate path.** The project is protected by Vercel
   Authentication for non-custom domains. `vercel curl` works interactively, but a Python
   acceptance client needs the `x-vercel-protection-bypass` header. The clients accept
   `--bypass`; the W8 script reads the secret from the project settings instead of committing it.
2. **Cold starts are visible.** The first hosted request after a scale-to-zero can take seconds;
   the clients use a 90–120 s timeout. `/ready` is the right gate before the source-control
   calls.
3. **The hosted branch is the same Neon branch as local acceptance.** That is deliberate for
   M0: one branch proves both paths against one database; the repository names separate the
   smoke runs. A separate branch per environment is a deployment decision, not a code change.
4. **The temporary repositories remain.** The work order's step 10 (delete the smoke repo) is
   not implemented. This is the W4 repository-deletion gap, now visible in the hosted run.
5. **No Git operation is involved.** The acceptance client speaks only HTTP and Scion ids; the
   deploy pipeline still uses Git/CLI for the ForgeVCS source, which the work order permits.
6. **Vercel routed a workspace request to a different container.** The second hosted W8 run
   created a workspace, then a later `diff` reached another instance and answered 404 because the
   registry was process-local. The fix persists `WcState` and the dirty-file overlay in Neon
   (`forgevcs_workspaces`, `forgevcs_workspace_files`) and restores them on demand.
7. **The repository registry had the same process-local assumption.** After the workspace fix,
   a repository initialized on one container answered 404 on the next request that reached
   another. `repo_engine` now checks the `repos` row in Neon and boots that engine on demand;
   a name with no row still answers 404, so a typo cannot create a repository. Both fixes came
   from running the hosted smoke, not from the local one, which is why W8 exists.

## 4. M0 checklist

| Requirement | Evidence |
| --- | --- |
| Carter Scion running behind our service boundary | W4 endpoints call the vendored handlers in-process; `upstream` unchanged except the `pub mod interceptor` visibility edit |
| Neon metadata | `meta_store_suite` 20/20; `/ready` `SELECT 1` from the Vercel container |
| Neon object storage | blob conformance; `/ready` bucket `HEAD`; W4/W5 upload/read |
| Independently deployed on Vercel | project `culebraluxe-forgevcs`; `/health`, `/ready`, `/version` 200 |
| Upstream conformance still passing | gate 1343 passed / 0 failed |
| Source-control operations without Git | W4/W5 hosted smoke, `W8_HOSTED=PASS` |
| Both security implementations retained | Scion policy in every handler; Culebra A/B gate in W6 (`SCION_ONLY` hosted) |

## 5. Remaining after M0

- Repository deletion (hosted smoke cleanup and the W4 gap).
- Custom domain and public exposure policy.
- Culebra identity mapping for agents/humans before `DUAL_ENFORCE`.
- Workspace persistence/affinity across scale-in (W5 gap).
- MCP read/write, Forge integration, review service, Git mirror — explicitly out of M0.
