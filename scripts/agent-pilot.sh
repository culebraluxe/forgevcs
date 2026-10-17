#!/usr/bin/env bash
# Boot a local ForgeVCS engine with three service principals and run the multi-agent pilot.
#
#   scripts/agent-pilot.sh                 # uses .env.local, port 3998
#   PORT=4100 scripts/agent-pilot.sh
#
# This is the M1 "a few bots at once" acceptance. It creates four repositories in the Neon
# branch behind .env.local; there is no repository deletion endpoint yet, so the client
# prints their names and they are cleaned up manually.
#
# It does not change acl.toml, the repo, or the hosted deployment. The three principals are
# throwaway secrets generated here, so nothing new is committed to the deployment's config.
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
# The pilot policy names agent-a/b/c, which the shipped acl.toml does not. It is a local
# test file so the hosted deployment's policy stays exactly as committed.
export FORGEVCS_POLICY_PATH="${FORGEVCS_POLICY_PATH:-$ROOT/scripts/pilot/acl.toml}"
export FORGEVCS_AUTH_MODE="${FORGEVCS_AUTH_MODE:-SCION_ONLY}"
export PORT="${PORT:-3998}"

# Throwaway pilot identities. The engine takes `name=<sha256 of secret>`; the client mints a
# token per name with the matching secret. Keep them deterministic so a rerun of one host does
# not accumulate principals.
SECRET_A="pilot-agent-a-secret"
SECRET_B="pilot-agent-b-secret"
SECRET_C="pilot-agent-c-secret"
hash_of() { printf '%s' "$1" | shasum -a 256 | cut -d' ' -f1; }
export FORGEVCS_SERVICE_PRINCIPALS="agent-a=$(hash_of "$SECRET_A"),agent-b=$(hash_of "$SECRET_B"),agent-c=$(hash_of "$SECRET_C")"

BASE="http://127.0.0.1:$PORT"
LOG="${TMPDIR:-/tmp}/forgevcs-agent-pilot-$PORT.log"

cargo build -q -p forgevcs-service || { echo "BUILD_FAILED"; exit 1; }
TARGET_DIR="${CARGO_TARGET_DIR:-$(cargo metadata --format-version 1 --no-deps |
  sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')}"
BIN="$TARGET_DIR/debug/forgevcs-service"
[ -x "$BIN" ] || { echo "no binary at $BIN" >&2; exit 1; }

"$BIN" >"$LOG" 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null' EXIT

ready=0
for _ in $(seq 1 60); do
  sleep 1
  if curl -fsS "$BASE/health" >/dev/null 2>&1; then ready=1; break; fi
done
if [ "$ready" -ne 1 ]; then
  echo "engine did not become healthy on $BASE; log tail:" >&2
  tail -40 "$LOG" >&2
  exit 1
fi

printf 'engine ready on %s\n\n' "$BASE"
python3 "$ROOT/scripts/agent-pilot.py" \
  --base "$BASE" \
  --agents "agent-a=$SECRET_A,agent-b=$SECRET_B,agent-c=$SECRET_C"
RC=$?

if [ "$RC" -ne 0 ]; then
  echo
  echo "engine log tail ($LOG):" >&2
  tail -60 "$LOG" >&2
fi
exit "$RC"
