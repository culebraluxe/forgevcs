# Vendored Scion patches

These files are carried on top of the pinned upstream tree. They are **not applied automatically**
by the build; the working copy already contains them. Use them when bumping `e0ffadf9…` or
re-vendoring Scion.

- `td-12-conflict-fail-closed.patch` — `filter_conflict_payload` fails closed when a conflicting
  path is writable but unreadable; see ForgeVCS `docs/TECH_DEBT.md` TD-12.

Apply from the ForgeVCS repository root after replacing `upstream/`:

```bash
git apply upstream/patches/td-12-conflict-fail-closed.patch
```

Then run the upstream gate and the ForgeVCS service tests. Keep this directory and
`README.md` updated when a patch is upstreamed or dropped.
