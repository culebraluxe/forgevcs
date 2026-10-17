# ForgeVCS — instructions for agents

**The project is ForgeVCS** (source-control engine: Carter's Scion, vendored). Repo root is
`/Users/Shared/dev/src/forgevcs`.

Layout: ForgeVCS is its own cargo workspace (`crates/forgevcs-*`) and its own Vercel
deploy (`Dockerfile.vercel`). `upstream/` is vendored Scion and is a **separate** cargo
workspace — the root manifest sets `exclude = ["upstream"]` so cargo resolves Scion's
inherited fields against Scion, not against us. Do not merge the two workspaces.

**Version control.** The root is a plain git repo; first commit `c37c343`, tagged
`w1-service-shell`. Carter runs **jj in front of git** on his other repos
(`Culebraluxe-web` is a jj+git colocated repo), so keep history linear and commits
described rather than terse. Identity is `culebradev <culebraluxe@gmail.com>`.
`upstream/` is committed as **plain files, not a submodule**, so the baseline SHA is
inspectable with no network. `.env.local` and `.neon/` are gitignored and must stay
that way — they hold live Neon credentials (`neon env pull` writes them, and rotates
them on every run).

Read these before changing anything, in this order:

1. [`docs/adr/0001-infrastructure-stack.md`](docs/adr/0001-infrastructure-stack.md) — the
   **binding** infrastructure decision: runtime, Vercel deployment shape, Neon Postgres,
   Neon Object Storage, the database-access ruling, module boundaries, the forbidden list,
   and the platforms verified on 2026-10-16 (§9).
2. [`docs/BASELINE.md`](docs/BASELINE.md) — the W0 acceptance record: upstream SHA, pinned
   toolchain, the gate command, measured T-core/T-full/T-external results, and the seams
   later workstreams exploit.
3. [`docs/W1-service-shell.md`](docs/W1-service-shell.md) — the service that exists today:
   routes, env config, and what is deliberately unfinished.
4. [`docs/W3-object-storage.md`](docs/W3-object-storage.md) — the object-storage work
   order mapped clause-by-clause onto the vendored source, including what is **already
   implemented** and must not be rebuilt.

## Hard rules

- **Do not substitute infrastructure.** Railway, AWS/GCP, Kubernetes, Redis, Kafka, NATS,
  Elasticsearch, extra worker services, and any additional database are out unless a
  measured requirement is written up as a new ADR and approved. If a step seems to need
  one, stop and write the ADR — do not quietly deviate.
- **WIP = 1.** One workstream in progress at a time. `W2` does not start until `W1`'s
  acceptance passes. No parallel "just getting started" branches.
- **`upstream/` is vendored Scion.** Adapt it at the edges (new crates, new adapters,
  aliases, a dialect module); do not rewrite its behaviour. Its `scripts/check.sh` must
  stay green.
- **Port, do not rewrite, the CockroachDB store** — ADR-0001 §7.1. The SERIALIZABLE
  transaction shape and the bounded `40001` retry loop already exist; re-deriving them is
  the expensive mistake.
- **Do not rebuild storage.** BLAKE3 hashing, zstd compression, content-addressed keys,
  idempotent puts and verify-on-read already exist in `scion-store`; see
  `docs/W3-object-storage.md` §1 before writing a line of `put`/`get`.
- **Canonical state lives in Neon only.** Local disk and Vercel Drives are cache/workspace.
  If a code path would make a filesystem authoritative, it is wrong.
- **Config comes from the environment.** No embedded secrets, no committed credentials.
  Neon Object Storage injects `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
  `AWS_ENDPOINT_URL_S3`, `AWS_REGION`; the service reads its own `FORGEVCS_*` variables and
  falls back to those.
- **Stay portable.** A boring `cargo build` → Docker → listen on `$PORT` binary. The only
  Vercel-specific artefact is the filename `Dockerfile.vercel`. No Vercel SDK calls in
  business logic; Daemons and Drives are later-phase, never V0 dependencies.

## Gate before claiming a workstream is done

```sh
upstream/scripts/check.sh             # vendored upstream gate — must exit 0
```

(Run it as `upstream/scripts/check.sh` from the repo root. The script re-roots itself with
`cd "$(dirname "$0")/.."`, so it does not exist at `scripts/check.sh` in this repo — only
inside `upstream/`.)

Gate on **T-core**. Do **not** gate on `check.sh --full`: it is red upstream at the
baseline SHA on one test at one hardcoded seed (`docs/BASELINE.md`, "T-full result"), so
T-full's rule is "no *new* failures versus that known set of one".

…plus the relevant Neon conformance run (`meta_store_suite` / `blob_store_suite` via their
crate test binaries) once a Neon backend exists.

