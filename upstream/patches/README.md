# Vendored Scion patches

These files are carried on top of the pinned upstream tree. They are **not applied automatically**
by the build; the working copy already contains them. Use them when bumping `e0ffadf9…` or
re-vendoring Scion.

- `td-12-conflict-fail-closed.patch` — `filter_conflict_payload` fails closed when a conflicting
  path is writable but unreadable; see ForgeVCS `docs/TECH_DEBT.md` TD-12.
- `merge-commit-v1-landing.patch` — adds the opt-in tip-parented merge-commit topology:
  `History::land_merge` + `LandMergeRequest` in `scion-history`, and the in-process
  `handlers::write::land_merge` entry point in `scion-server` (no proto route). It lands a
  two-parent `Merge(tip, head)` changeset as itself only while its first parent is the ref tip,
  with a single `landed` row; stale tips and conflicts are typed refusals, nothing is rebased,
  squashed or paused. See ForgeVCS `docs/MERGE-TOPOLOGY.md`.

Apply from the ForgeVCS repository root after replacing `upstream/`:

```bash
git apply upstream/patches/td-12-conflict-fail-closed.patch
git apply upstream/patches/merge-commit-v1-landing.patch
```

Then run the upstream gate and the ForgeVCS service tests. Keep this directory and
`README.md` updated when a patch is upstreamed or dropped.
