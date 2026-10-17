# W7 — Separate Vercel deployment

**Status:** complete 2026-10-17. ForgeVCS runs as its own Vercel project, a container service
built from `Dockerfile.vercel`, on Neon Postgres + Neon Object Storage.

- **Project:** `culebraluxe-forgevcs` (team `culebraluxe-4460s-projects`)
- **Production URL:** https://culebraluxe-forgevcs.vercel.app
- **Deployment:** CLI (`vercel deploy --prod`), not Git-connected yet

## 1. Why a separate project

The work order requires that a ForgeVCS failure cannot affect CulebraLuxe production. The
service therefore has its own Vercel project, environment, build, deployment history, logs,
domain and rollback. It is not a route inside `culebraluxe-web-fp`.

## 2. Runtime shape

```text
Vercel project culebraluxe-forgevcs
        │
        └── service "forgevcs" (runtime: container)
                │  root: . · entrypoint: Dockerfile.vercel
                ▼
        forgevcs-service :3000
                │
                ├── Neon Postgres (metadata, migrations, A/B decisions)
                └── Neon Object Storage (content-addressed objects)
```

`vercel.json` declares the service and the catch-all rewrite:

```json
{
  "services": {
    "forgevcs": {
      "root": ".",
      "entrypoint": "Dockerfile.vercel",
      "runtime": "container"
    }
  },
  "rewrites": [
    { "source": "/(.*)", "destination": { "service": "forgevcs" } }
  ]
}
```

`Dockerfile.vercel` builds a release binary in `rust:1.98-bookworm` (with `protobuf-compiler`
for Scion's proto build), copies the binary and `acl.toml` into a slim Debian runtime, and runs
as uid 10001. The policy is at `/etc/forgevcs/acl.toml` and `FORGEVCS_POLICY_PATH` points there.

The runtime image does **not** contain a compiler, Cargo or the upstream source tree. Local
Docker and Vercel run the same portable binary; only the filename is Vercel-specific.

## 3. Environment

All values are Vercel Production secrets; none are in the repository.

| Variable | Purpose |
| --- | --- |
| `FORGEVCS_DATABASE_URL` | Neon unpooled URL (migrations and the metadata pool) |
| `FORGEVCS_BUCKET` | `forgevcs-objects` on the linked branch |
| `FORGEVCS_AWS_ENDPOINT_URL_S3` | Branch-scoped Neon Object Storage endpoint |
| `FORGEVCS_AWS_REGION` | `aws-us-east-2` |
| `FORGEVCS_AWS_ACCESS_KEY_ID` / `FORGEVCS_AWS_SECRET_ACCESS_KEY` | Branch-scoped object credentials |
| `FORGEVCS_POLICY_PATH` | `/etc/forgevcs/acl.toml` |
| `FORGEVCS_AUTH_MODE` | `SCION_ONLY` for the first hosted deployment |
| `FORGEVCS_SERVICE_PRINCIPALS` | `maestro=<sha256>` for the hosted smoke principal |
| `FORGEVCS_SIGNING_KEY_PEM` | PKCS#8 Ed25519 PEM; the engine writes it to a private temp file, loads it, then deletes it |
| `FORGEVCS_REPO` | `forgevcs` |
| `PORT` | `3000`; the container runs non-root, so it cannot bind Vercel's default 80 |
| `RUST_LOG` | `info` |

The service principal secret is kept outside the repo at
`/Users/Shared/dev/.forgevcs-vercel-maestro.secret` (mode 600), and the signing key at
`/Users/Shared/dev/.forgevcs-signing-key.pem` (mode 600). The Vercel project holds the
authoritative copies.

## 4. Acceptance

```sh
vercel deploy --prod --yes
vercel curl https://culebraluxe-forgevcs.vercel.app/health
vercel curl https://culebraluxe-forgevcs.vercel.app/ready
vercel curl https://culebraluxe-forgevcs.vercel.app/version
```

Observed:

```
/health   HTTP 200  {"service":"forgevcs","status":"ok","version":"0.1.0"}
/ready    HTTP 200  {"service":"forgevcs","status":"ready","version":"0.1.0"}
/version  HTTP 200  {"service":"forgevcs","status":"ok","upstream":"e0ffadf9…","version":"0.1.0"}
```

`/ready` is 200 only after the container has loaded the policy, connected to Neon, run/checked
the schema and answered the bucket `HEAD`; it is the deployment's real dependency check, not a
platform "successful" message.

## 5. Findings

1. **A `Dockerfile.vercel` alone was not enough.** The first deploy ran `vercel build` in 254 ms
   and produced a static shell. The container runtime needs the explicit `services` entry and
   catch-all rewrite in `vercel.json`; after that, Vercel built the image with buildah and pushed
   it to VCR.
2. **The Rust image does not ship `protoc`.** `scion-proto`'s build script failed with
   "Could not find `protoc`"; the build stage now installs `protobuf-compiler`.
3. **The policy file was copied into the build stage only.** The first container image had no
   `/etc/forgevcs/acl.toml`, so `/ready` returned 503 with the exact missing path. The runtime
   stage now copies it too.
4. **Port 80 is not bindable by the non-root user.** The image and the Vercel project set
   `PORT=3000`; the service stays `USER 10001` and Vercel routes to the declared service.
5. **Signing keys need an environment path on Vercel.** Vercel injects secrets as values, not
   files. `FORGEVCS_SIGNING_KEY_PEM` is written to a private temp file by `engine.rs`, parsed by
   the same `Keyring::from_files` path, and removed. With this set, tokens survive a redeploy.
6. **Deployment Protection is on.** The project uses Vercel Authentication for non-custom
   domains; `vercel curl` and the automation-bypass header reach it. The public custom domain is
   not configured yet.

## 6. Remaining gaps

- **Custom domain** `forgesc.culebraluxe.com` (or `forgevcs.culebraluxe.com`) is not attached;
  the generated `culebraluxe-forgevcs.vercel.app` is the current address.
- **Git integration** is not connected; deployments are CLI-driven. Connecting the repo would
  give push-to-deploy and previews, but is a project decision.
- **Preview environments** have no environment variables yet; only Production is configured.
- **Repository deletion** is still absent, so hosted smoke runs leave their temporary repos in
  the branch.
- **One warm container, one workspace registry.** Vercel scale-in discards workspace IDs; see
  W5 §7.
- **Rate limiting/load shedding** remain platform concerns until the gRPC layers are reused at
  the façade (W4 §8.6).
