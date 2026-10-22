# Merge engine: select, validate, report, and record

**Status:** config/validation/reporting and land-response/log evidence shipped 2026-10-22 (Lane W);
**recording in repository history is blocked upstream** (exact blocker below). This document is the
single place operators should read before changing `FORGEVCS_MERGE_ENGINE`.

## Why this matters

Merge/rebase results are deterministic for identical `(a, b, merge engine version)` inputs
(`docs/M1-merge-analysis.md` §1). The opt-in rename-aware engine changes that result set: with it
off, a rename is delete + add; with `exact-rename-v1` on, an edit can follow a rename instead of
producing a false `modify_delete` conflict. The vendored engine reads the variable **directly from
the process environment inside the merge/rebase path** (`upstream/crates/scion-history/src/rename.rs:34-40`,
consulted from `merge.rs:122` and `rebase.rs:80`) — so two deployments, or two instances with
different environments, can produce different changeset identities for the same operation. The
variable was previously undocumented in any deploy path and was never reported by the service.

## Configuration (validated)

`FORGEVCS_MERGE_ENGINE` is mapped into `Config` (`crates/forgevcs-service/src/config.rs:76,123`) and
validated at readiness (`config.rs:196`):

| value | meaning |
|---|---|
| unset / empty | `default` — upstream delete+add rename behaviour |
| `default` | same as unset |
| `exact-rename-v1` | opt-in exact/similarity rename-aware merge engine (TD-1) |
| anything else | **config error** — `/ready` fails with `FORGEVCS_MERGE_ENGINE must be 'default' or 'exact-rename-v1'` |

A typo no longer silently runs the default engine. Tests:
`config::tests::merge_engine_is_validated_and_reported` (default, both valid values, typo, blank).

## Reporting and evidence

- **`GET /version`** reports `"merge_engine": "default" | "exact-rename-v1"`
  (`crates/forgevcs-service/src/http.rs:138`; test `version_reports_the_configured_merge_engine`).
  This is the canonical way to check what a deployment is actually running.
- **Startup log** carries `merge_engine=` (`main.rs:65`).
- **Push/land evidence**: successful `POST /repos/{repo}/push` (mode land/update), `POST /land` and
  `POST /merge-land` responses include `"merge_engine"` alongside `ref`/`landed`
  (`facade/mod.rs:1266,1558,1821`), and each successful landing writes a structured log line
  `landed repo=… ref_name=… target=… version=… merge_engine=…` (`facade/mod.rs:1215,1549,1812`).
  That is the durable operational evidence available today; it is **not** embedded in the
  repository's history objects.

## Deployment requirement (operator step + verification)

The hosted ForgeVCS project deploys a **prebuilt container** from a staging directory
(`scripts/deploy-forgevcs-prod.sh` step 4); it does not read this repository's `vercel.json`, so the
merge engine is **project-level Vercel environment**, not repo-controlled config. Required:

1. Vercel project `culebraluxe-forgevcs` → Environment Variables:
   `FORGEVCS_MERGE_ENGINE=exact-rename-v1` for Production (and Preview if previews are used).
2. After any deploy, verify:
   `curl -s https://culebraluxe-forgevcs.vercel.app/version | jq .merge_engine` → `"exact-rename-v1"`.
   `scripts/deploy-forgevcs-prod.sh` already prints `/version` in its final health loop; the value
   is now visible there. A deploy with the wrong/missing value must be treated as a different merge
   engine, not a cosmetic difference.
3. Local acceptance/CI must export the same value when reproducing hosted merge behaviour
   (`scripts/agent-pilot.sh:35` already defaults it; other local harnesses run `default` unless set).

**Blocker for repo-controlled deployment config:** ForgeVCS uses a container deploy with git
deployment disabled (`vercel.json`), so there is no checked-in file that can set the runtime env.
Options for a future change: move env into the deploy script via `vercel env add` (requires a
Vercel token in CI) or attach a project configuration export to the deploy receipt. Until then this
is an explicit operator step + `/version` verification, documented in `docs/W7-vercel-deployment.md`.

## Should production fail `/ready` when `FORGEVCS_MERGE_ENGINE` is unset? (evaluation, 2026-10-22)

**Recommendation: no — keep `/ready` a dependency probe; enforce the deployment declaration in the
release path instead.** Reasons:

1. **Unset is a supported engine, not an error.** It is exactly `default` (upstream delete+add), and
   that engine is a legitimate choice (compatibility with old history, deliberately conservative
   merges). Failing readiness would turn a valid configuration into a 503.
2. **`/ready` has a defined meaning** (ADR-0002 D5): can this instance serve requests (policy
   loaded, DB/bucket reachable)? Coupling it to a merge-policy choice conflates dependency health
   with deployment intent, and would take every instance out of rotation on a config omission
   instead of surfacing the wrong engine while serving.
3. **Local/CI/preview environments intentionally run the default.** A hard-required variable would
   force every test harness and preview deployment to set it or fail readiness, for no safety gain
   (test repos are disposable).
4. **The real risk is silent semantic drift, and it is now visible**: `/version` reports
   `merge_engine`, the startup log records it, and every land logs it; the operator checklist
   (`docs/W7-vercel-deployment.md`) requires `exact-rename-v1` in the Vercel project and verifies
   `/version`. A release that cannot show the expected value should not be promoted.
5. **If the principal wants strictness later**, add a deploy-policy check (CI/deploy script asserts
   `/version.merge_engine` before promotion) or an explicit opt-in
   `FORGEVCS_REQUIRE_EXPLICIT_MERGE_ENGINE=1` that fails readiness only in production; do not make
   the unset default itself fatal.

**Status:** documented decision only; no `/ready` code change in task-87. This is still a principal
decision if they want the hard-required variant; the recommended default is to require it in the
release checklist, not in readiness.

## Recording the engine version in repository history — exact upstream blocker

The façade cannot stamp the engine version into the changesets that land writes, because those
changesets are created inside the vendored engine:

- `History::rebase` creates the result with **the source changeset's meta**
  (`upstream/crates/scion-history/src/rebase.rs:104-108`, `MetaSource::Existing(cs.meta)`); `Land`
  rebases each stack member through this path, so a rebased/landed changeset inherits the original
  meta and the façade never sees a hook to add a tag.
- `History::merge` creates its result from **caller-supplied** `MetaFields`
  (`upstream/crates/scion-history/src/merge.rs:46,143`, `MetaSource::New(meta)`); only the explicit
  `merge`/`merge-land` request meta reaches it, and a client-supplied value is not authoritative.
- `scion-object` Meta extensions could carry an engine tag: extensions are `(tag, value)` pairs with
  even tags ignorable (`upstream/crates/scion-object/src/extensions.rs:7-11,44-58`), currently tags
  `2` (git commit) and `4` (skipped gitlinks). Adding a new tag means defining it in SPEC, stamping
  it in `rebase.rs`/`merge.rs` (and therefore a vendored-engine change + upstream gate), and defining
  the engine-version semantics that `land` records.

Per the work order this is **not forced**: an upstream engine change is required, with its own
engine-version mechanism and differential tests. Interim behaviour is the `/version` + startup-log +
land-response/log evidence above. Closing criterion: an ignorable Meta extension tag (or an
engine-version field in the land/ref ledger) written by the engine for every generated changeset,
reported by `/version`, with tests proving a changed engine value is visible on the resulting
changeset/land record.

## Residual risks

- A deployment that omits the variable silently runs the default engine (now visible, but
  `/version` must be checked as part of deploy verification).
- The engine reads the environment at call time; runtime env mutation (not a normal ForgeVCS path)
  could desynchronise the validator from the engine. Service processes are restarted on config
  change by design.
- The engine version is not in history, so cross-deployment history audits cannot prove which engine
  produced an old changeset until the upstream blocker is closed.
