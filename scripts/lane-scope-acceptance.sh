#!/usr/bin/env bash
# Live acceptance for the narrowed lane policy (R3): `scripts/pilot/acl-lanes.toml`.
#
#   scripts/lane-scope-acceptance.sh
#   PORT=4140 scripts/lane-scope-acceptance.sh
#
# Boots ONE local service process against the Neon branch in .env.local (the same branch the W6
# and role-pilot acceptances use; no deploy, no hosted call), then runs
# scripts/lane-scope-acceptance.py, which proves over HTTP:
#
#   * devops can still provision lane refs (admin.refs intact),
#   * agent-a push under lane/a/** is allowed, under lane/b/** and src/** is 403,
#   * agent-b push under lane/b/** is allowed (the refusal is scoping, not a global deny),
#   * agent-a read/write of another repository is 403 (repository ACL),
#   * the human `forgevcs` CLI still mints, reads, pushes a root path, pulls and uses a workspace.
#
# The two repositories are created and deleted by the client (pass or fail). Audit rows in
# forgevcs_auth_decisions are the intended live evidence and are not removed. No migration runner
# is involved: the policy change is a file the single local process loads at boot.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 2

if [ ! -f .env.local ]; then
  echo "no .env.local: run 'neon env pull' first" >&2
  exit 2
fi
set -a
# shellcheck disable=SC1091
. ./.env.local
set +a

# Safety: this script deletes the repositories it names, so never run it against production.
BRANCH=$(python3 -c 'import json; print(json.load(open(".neon")).get("branch", ""))' 2>/dev/null || true)
if [ -n "$BRANCH" ] && [[ "$BRANCH" == *prod* ]] && [ "${LANE_SCOPE_ALLOW_PRODUCTION:-0}" != "1" ]; then
  echo "refusing to run against Neon branch '$BRANCH' (set LANE_SCOPE_ALLOW_PRODUCTION=1 to override)" >&2
  exit 2
fi
echo "neon branch: ${BRANCH:-<unknown>}"

export FORGEVCS_DATABASE_URL="${DATABASE_URL_UNPOOLED:-$DATABASE_URL}"
export FORGEVCS_BUCKET="${FORGEVCS_BUCKET:-forgevcs-objects}"
export FORGEVCS_POLICY_PATH="$ROOT/scripts/pilot/acl-lanes.toml"
export FORGEVCS_AUTH_MODE="${FORGEVCS_AUTH_MODE:-SCION_ONLY}"
PORT="${PORT:-4132}"
export PORT
BASE="http://127.0.0.1:$PORT"

STAMP="$(date +%y%m%d%H%M%S)$(openssl rand -hex 2)"
REPO_A="lane-pilot-a-$STAMP"
REPO_B="lane-pilot-b-$STAMP"

umask 077
SECRET_AGENT_A="$(openssl rand -hex 16)"
SECRET_AGENT_B="$(openssl rand -hex 16)"
SECRET_DEVOPS="$(openssl rand -hex 16)"
HUMAN_SECRET="$(openssl rand -hex 16)"
hash_of() { printf '%s' "$1" | shasum -a 256 | cut -d' ' -f1; }

export FORGEVCS_SERVICE_PRINCIPALS="agent-a=$(hash_of "$SECRET_AGENT_A"),agent-b=$(hash_of "$SECRET_AGENT_B"),devops=$(hash_of "$SECRET_DEVOPS")"
export FORGEVCS_HUMAN_PRINCIPALS="{\"culebraluxe\":{\"email\":\"culebraluxe@gmail.com\",\"display\":\"Culebraluxe\",\"secret_sha256\":\"$(hash_of "$HUMAN_SECRET")\"}}"
export FORGEVCS_REPO_ACLS="{\"$REPO_A\":{\"owner\":\"service:devops\",\"writers\":[\"service:agent-a\",\"service:agent-b\"]},\"$REPO_B\":{\"owner\":\"service:devops\"}}"

export LANE_AGENT_A_SECRET="$SECRET_AGENT_A"
export LANE_AGENT_B_SECRET="$SECRET_AGENT_B"
export LANE_DEVOPS_SECRET="$SECRET_DEVOPS"

TMP_DIR=$(mktemp -d)
HUMAN_SECRET_FILE="$TMP_DIR/human-secret"
printf '%s' "$HUMAN_SECRET" >"$HUMAN_SECRET_FILE"
chmod 600 "$HUMAN_SECRET_FILE"
export LANE_HUMAN_SECRET_FILE="$HUMAN_SECRET_FILE"
CLI_CONFIG_DIR="$TMP_DIR/cli"
mkdir -p "$CLI_CONFIG_DIR"
LOG="$TMP_DIR/service.log"

SERVICE_PID=""
cleanup() {
  [ -n "$SERVICE_PID" ] && kill "$SERVICE_PID" 2>/dev/null
  wait "$SERVICE_PID" 2>/dev/null
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

if [ "${LANE_SCOPE_SKIP_BUILD:-0}" != "1" ]; then
  cargo build -q -p forgevcs-service -p forgevcs-cli || { echo "BUILD_FAILED"; exit 1; }
else
  echo "SKIP_BUILD=1: using the existing binaries in the target directory (provenance must be stated)"
fi
TARGET_DIR="${CARGO_TARGET_DIR:-$(cargo metadata --format-version 1 --no-deps |
  sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')}"
SERVICE_BIN="$TARGET_DIR/debug/forgevcs-service"
CLI_BIN="$TARGET_DIR/debug/forgevcs"
[ -x "$SERVICE_BIN" ] || { echo "no service binary at $SERVICE_BIN" >&2; exit 1; }
[ -x "$CLI_BIN" ] || { echo "no CLI binary at $CLI_BIN" >&2; exit 1; }

"$SERVICE_BIN" >"$LOG" 2>&1 &
SERVICE_PID=$!

# Wait for /ready (DB + bucket), not just /health: init/push need storage.
for _ in $(seq 1 60); do
  sleep 1
  if curl -fsS "$BASE/ready" >/dev/null 2>&1; then break; fi
done
if ! curl -fsS "$BASE/ready" >/dev/null 2>&1; then
  echo "engine did not become ready on $BASE; log tail:" >&2
  tail -60 "$LOG" >&2
  exit 1
fi
echo "engine ready on $BASE (single local instance, policy scripts/pilot/acl-lanes.toml)"

python3 "$ROOT/scripts/lane-scope-acceptance.py" \
  --base "$BASE" \
  --repo-a "$REPO_A" \
  --repo-b "$REPO_B" \
  --cli "$CLI_BIN" \
  --config-dir "$CLI_CONFIG_DIR"
STATUS=$?

if [ "$STATUS" != 0 ]; then
  echo "service log tail:" >&2
  tail -40 "$LOG" >&2
fi
exit "$STATUS"
