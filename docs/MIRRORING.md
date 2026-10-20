# Mirroring into ForgeVCS

ForgeVCS is the source-control home for the three repos, but **git remains the source while work happens**.

| ForgeVCS repo | Git checkout | Notes |
| --- | --- | --- |
| `culebraluxe-web` | `/Users/Shared/dev/src/lane-deep` | source mirror **excludes `public/`** (126 binary assets, 66 MB); everything else verified |
| `forge` | `/Users/Shared/dev/src/forge` | full tree |
| `workflow` | `/Users/Shared/dev/src/workflow` | full tree |

## Commands

```bash
bash scripts/mirror-all.sh            # mirror all three
bash scripts/mirror-all.sh --check    # exit 3 if any repo drifted; no writes
```

`--check` compares the current git tree with the mirror's `main`; exit 3 means one or more repos need a mirror. That is the drift alarm; wire it into a daily job if wanted.

## Why the app is source-only

`public/` is 65.9 MB of images. The hosted Vercel endpoint rejects a body over ~4.5 MB; the mirror
batches pushes, but mirroring 66 MB of binaries on every run is not worth the transfer. The exclusion is
explicit and reported by the mirror (`excluded: ["public/"]`); code, docs, tests, scripts and lockfiles
are mirrored and sha256-verified.

## Credentials

`scripts/mirror-all.sh` uses the same hosted service token as the W8 smoke:
`FORGEVCS_SECRET_FILE` (default `/Users/Shared/dev/.forgevcs-vercel-maestro.secret`) and the Vercel
protection-bypass header read from `vercel project protection`. Never commit the secret.

## Known limits

- Commit → changeset **identity** is not preserved (WIRE-3 importer); the mirror verifies content/tree ids.
- Re-running with an unchanged tree is a NOOP.
- The app mirror's root tree id is computed from the filtered tree, so it matches the mirror, not git's full tree.
