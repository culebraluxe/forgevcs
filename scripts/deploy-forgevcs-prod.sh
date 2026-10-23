#!/usr/bin/env bash
# Production deploy for the SEPARATE ForgeVCS Vercel project.
#
# Follows the CulebraLuxe-house release shape (scripts/deploy-prod.sh in lane-deep):
#   1. compile locally in Docker for Vercel's linux/amd64
#   2. stage a tiny runtime image (binary + acl.toml, no compiler)
#   3. PATCH the project to framework=container
#   4. deploy the stage; Vercel builds only the runtime layer
#   5. ask the live service whether it is ready, then optionally run W8
#
# It deliberately never touches culebraluxe-web-fp.
set -euo pipefail

PROJECT="culebraluxe-forgevcs"
PROJECT_ID="prj_JdIzTs6dXi9EWXmmlDoQrTL2tGLQ"
TEAM="culebraluxe-4460s-projects"
TEAM_ID="team_xk8vFaeSyY6CuSkS3OK55tTc"
URL="${FORGEVCS_HOSTED_URL:-https://culebraluxe-forgevcs.vercel.app}"
VERCEL_CLI_VERSION="${VERCEL_CLI_VERSION:-64.5.0}"

vc() { npx --yes "vercel@${VERCEL_CLI_VERSION}" "$@"; }
fail() { printf '\nERROR: %s\n' "$1" >&2; exit 1; }

ROOT_DIR="$(git rev-parse --show-toplevel 2>/dev/null)" || fail "Run this inside the ForgeVCS repository"
cd "$ROOT_DIR"

[[ "$(git branch --show-current)" == "main" ]] || fail "Production deploys must run from main."
[[ -z "$(git status --porcelain)" ]] || fail "Working tree is not clean. Commit or discard changes first."
command -v docker >/dev/null 2>&1 || fail "Docker is required for the local compile."
docker info >/dev/null 2>&1 || fail "Docker is not running. Start Docker Desktop."
vc whoami >/dev/null 2>&1 || fail "Vercel CLI is not authenticated. Run: vercel login."

printf '\nForgeVCS production deploy\n  project: %s\n  team:    %s\n  commit:  %s\n\n' \
  "$PROJECT" "$TEAM" "$(git rev-parse --short HEAD)"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/build" "$WORK/stage"

printf '1/5 Compiling locally for linux/amd64 (first run is slow; later runs reuse the cache)...\n'
docker build -f devops/Dockerfile.build --output "type=local,dest=$WORK/build" . \
  || fail "The local compile failed."
BIN="$WORK/build/forgevcs-service"
[ -f "$BIN" ] || fail "The compile produced no forgevcs-service binary."
file "$BIN" | grep -q 'x86-64' || fail "The binary is not linux/amd64: $(file "$BIN")"
printf '  binary: %s\n' "$(du -h "$BIN" | cut -f1)"

printf '\n2/5 Packing the runtime stage...\n'
cp devops/Dockerfile.runtime "$WORK/stage/Dockerfile"
gzip -9 -c "$BIN" > "$WORK/stage/forgevcs-service.gz"
cp acl.toml "$WORK/stage/acl.toml"
chmod -R a+rX "$WORK/stage"
printf '  upload size: %s\n' "$(du -sh "$WORK/stage" | cut -f1)"

printf '\n3/5 Making sure the project runs the application container...\n'
printf '{"framework":"container"}' > "$WORK/project.json"
vc api "/v9/projects/${PROJECT_ID}?teamId=${TEAM_ID}" -X PATCH --input "$WORK/project.json" >/dev/null \
  || fail "Could not set the project to run a container."

printf '\n4/5 Deploying the prebuilt runtime image...\n'
DEPLOY_OUT="$(
  cd "$WORK/stage" \
    && VERCEL_ORG_ID="$TEAM_ID" VERCEL_PROJECT_ID="$PROJECT_ID" vc deploy --prod --yes
)"
printf '%s\n' "$DEPLOY_OUT" | tail -8
DEPLOY_URL="$(printf '%s\n' "$DEPLOY_OUT" |
  grep -oE 'https://[^[:space:]]+\.vercel\.app' | head -1)"
[[ -n "$DEPLOY_URL" ]] || DEPLOY_URL="$URL"
printf '  deployment: %s\n' "$DEPLOY_URL"

printf '\n5/5 Checking the live service...\n'
READY=0
for _ in $(seq 1 30); do
  BODY="$(vc curl "${URL%/}/ready" 2>/dev/null || true)"
  printf '%s' "$BODY" | grep -q '"status":"ready"' && { READY=1; break; }
  sleep 5
done
[ "$READY" = 1 ] || fail "Deployed, but /ready never reported ready. Production is not healthy."

for path in /health /ready /version; do
  BODY="$(vc curl "${URL%/}${path}" 2>/dev/null || true)"
  printf '  %-9s %s\n' "$path" "$BODY"
  printf '%s' "$BODY" | grep -q "\"status\":\"$( [ "$path" = /ready ] && echo ready || echo ok )\"" \
    || fail "Live check failed for ${path}."
done

# Release guard: a deploy must not silently change merge semantics or drop the local
# vendored patch that closes the writable-but-unreadable conflict case.
VERSION_BODY="$(vc curl "${URL%/}/version" 2>/dev/null || true)"
EXPECTED_MERGE_ENGINE="${FORGEVCS_EXPECTED_MERGE_ENGINE:-exact-rename-v1}"
printf '%s' "$VERSION_BODY" | grep -q "\"merge_engine\":\"${EXPECTED_MERGE_ENGINE}\"" \
  || fail "Live /version does not report merge_engine=${EXPECTED_MERGE_ENGINE}."
printf '%s' "$VERSION_BODY" | grep -q '"td-12-conflict-fail-closed"' \
  || fail "Live /version is missing the TD-12 conflict-policy vendor patch."

printf '\nFORGEVCS DEPLOY COMPLETE — %s is ready.\n' "$URL"
printf 'Vercel built the runtime image only; the Rust workspace was compiled on this Mac.\n'
printf 'Rollback: Vercel -> %s -> Deployments -> promote the previous ready deployment.\n' "$PROJECT"

if [[ "${1:-}" == "--smoke" ]]; then
  printf '\nRunning the hosted W8 source-control smoke...\n'
  bash "$ROOT_DIR/scripts/w8-hosted-smoke.sh"
fi
