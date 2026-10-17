#!/usr/bin/env bash
# Production deploy for the SEPARATE ForgeVCS Vercel project.
#
# This deliberately does not touch culebraluxe-web-fp. It follows the same release
# conventions as scripts/deploy-prod.sh in the Culebraluxe-web tree: clean main, pinned
# CLI, one named project, deploy, then ask the live service whether it is actually ready.
# The image is currently built by Vercel from Dockerfile.vercel; the optional local
# prebuilt path (the main app's Dockerfile.build -> Dockerfile.runtime pattern) can
# replace that step without changing this guard/check shape.
set -euo pipefail

PROJECT="culebraluxe-forgevcs"
TEAM="culebraluxe-4460s-projects"
URL="${FORGEVCS_HOSTED_URL:-https://culebraluxe-forgevcs.vercel.app}"
VERCEL_CLI_VERSION="${VERCEL_CLI_VERSION:-64.5.0}"

vc() { npx --yes "vercel@${VERCEL_CLI_VERSION}" "$@"; }
fail() { printf '\nERROR: %s\n' "$1" >&2; exit 1; }

ROOT_DIR="$(git rev-parse --show-toplevel 2>/dev/null)" || fail "Run this inside the ForgeVCS repository"
cd "$ROOT_DIR"

[[ "$(git branch --show-current)" == "main" ]] || fail "Production deploys must run from main."
[[ -z "$(git status --porcelain)" ]] || fail "Working tree is not clean. Commit or discard changes first."
vc whoami >/dev/null 2>&1 || fail "Vercel CLI is not authenticated. Run: vercel login"

printf '\nForgeVCS production deploy\n  project: %s\n  team:    %s\n  commit:  %s\n\n' \
  "$PROJECT" "$TEAM" "$(git rev-parse --short HEAD)"

DEPLOY_OUT="$(vc deploy --prod --yes)"
printf '%s\n' "$DEPLOY_OUT" | tail -8

DEPLOY_URL="$(printf '%s\n' "$DEPLOY_OUT" |
  grep -oE 'https://[^[:space:]]+\.vercel\.app' | head -1)"
[[ -n "$DEPLOY_URL" ]] || DEPLOY_URL="$URL"
printf 'Deployment: %s\n' "$DEPLOY_URL"

printf '\nWaiting for the production alias to answer /ready...\n'
READY=0
for _ in $(seq 1 30); do
  BODY="$(vc curl "${URL%/}/ready" 2>/dev/null || true)"
  printf '%s' "$BODY" | grep -q '"status":"ready"' && { READY=1; break; }
  sleep 5
done
[ "$READY" = 1 ] || fail "Deployed, but /ready never reported ready. Production is not healthy."

for path in /health /ready /version; do
  BODY="$(vc curl "${URL%/}${path}" 2>/dev/null || true)"
  printf '%-10s %s\n' "$path" "$BODY"
  printf '%s' "$BODY" | grep -q "\"status\":\"$( [ "$path" = /ready ] && echo ready || echo ok )\"" \
    || fail "Live check failed for ${path}."
done

printf '\nFORGEVCS DEPLOY COMPLETE — %s is ready.\n' "$URL"
printf 'Rollback: Vercel -> %s -> Deployments -> promote the previous ready deployment.\n' "$PROJECT"

if [[ "${1:-}" == "--smoke" ]]; then
  printf '\nRunning the hosted W8 source-control smoke...\n'
  bash "$ROOT_DIR/scripts/w8-hosted-smoke.sh"
fi
