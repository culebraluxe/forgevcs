#!/usr/bin/env bash
# Boot a local ForgeVCS engine with a worker and the DevOps seat, and prove the authority
# split (scripts/pilot/acl-roles.toml):
#
#   scripts/role-pilot.sh
#   PORT=4100 scripts/role-pilot.sh
#
# Creates one repository (the worker's blocked attempt creates none); there is no deletion
# endpoint yet, so it stays until manual cleanup. Does not touch acl.toml or a deployment.
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

export FORGEVCS_DATABASE_URL="${DATABASE_URL_UNPOOLED:-$DATABASE_URL}"
export FORGEVCS_BUCKET="${FORGEVCS_BUCKET:-forgevcs-objects}"
export FORGEVCS_POLICY_PATH="${FORGEVCS_POLICY_PATH:-$ROOT/scripts/pilot/acl-roles.toml}"
export FORGEVCS_AUTH_MODE="${FORGEVCS_AUTH_MODE:-SCION_ONLY}"
export PORT="${PORT:-3997}"

SECRET_WORKER="role-pilot-agent-a-secret"
SECRET_DEVOPS="role-pilot-devops-secret"
hash_of() { printf '%s' "$1" | shasum -a 256 | cut -d' ' -f1; }
export FORGEVCS_SERVICE_PRINCIPALS="agent-a=$(hash_of "$SECRET_WORKER"),devops=$(hash_of "$SECRET_DEVOPS")"

BASE="http://127.0.0.1:$PORT"
LOG="${TMPDIR:-/tmp}/forgevcs-role-pilot-$PORT.log"

cargo build -q -p forgevcs-service || { echo "BUILD_FAILED"; exit 1; }
TARGET_DIR="${CARGO_TARGET_DIR:-$(cargo metadata --format-version 1 --no-deps |
  sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')}"
BIN="$TARGET_DIR/debug/forgevcs-service"
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

"$BIN" >"$LOG" 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null' EXIT

for _ in $(seq 1 60); do
  sleep 1
  if curl -fsS "$BASE/health" >/dev/null 2>&1; then break; fi
done
curl -fsS "$BASE/health" >/dev/null 2>&1 || {
  echo "engine did not become healthy on $BASE; log tail:" >&2
  tail -40 "$LOG" >&2
  exit 1
}

printf 'engine ready on %s\n\n' "$BASE"
python3 "$ROOT/scripts/role-pilot.py" \
  --base "$BASE" \
  --agents "agent-a=$SECRET_WORKER,devops=$SECRET_DEVOPS"
RC=$?

if [ "$RC" -ne 0 ]; then
  echo
  echo "engine log tail ($LOG):" >&2
  tail -60 "$LOG" >&2
fi
exit "$RC"
