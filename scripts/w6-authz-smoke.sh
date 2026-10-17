#!/usr/bin/env bash
# W6 acceptance: Scion and Culebra evaluate the same request, in shadow and dual modes.
#
#   scripts/w6-authz-smoke.sh
#
# Starts two short-lived processes against the live Neon branch:
#   SHADOW_COMPARE  a service principal's read is allowed by Scion, denied by Culebra,
#                   recorded as a mismatch, and the request still succeeds
#   DUAL_ENFORCE    the same read is denied because Culebra denies it
# Then it checks the persisted decisions and the invalid-mode readiness gate.
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
export FORGEVCS_POLICY_PATH="${FORGEVCS_POLICY_PATH:-$ROOT/acl.toml}"
export FORGEVCS_SERVICE_PRINCIPALS="maestro=$(printf '%s' 'smoke-only' | shasum -a 256 | cut -d' ' -f1)"
export PORT="${PORT:-4130}"
DB="${DATABASE_URL_UNPOOLED:-$DATABASE_URL}"
FAIL=0
SHADOW_PID=
DUAL_PID=
BAD_PID=

cleanup() {
  for pid in "$SHADOW_PID" "$DUAL_PID" "$BAD_PID"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null
  done
  wait 2>/dev/null
}
trap cleanup EXIT

wait_health() {
  local port=$1
  for _ in $(seq 1 30); do
    sleep 1
    curl -fsS "http://127.0.0.1:$port/health" >/dev/null 2>&1 && return 0
  done
  return 1
}

mint() {
  curl -sS -X POST "http://127.0.0.1:$1/auth/token" \
    -H 'content-type: application/json' \
    -d '{"service":"maestro","secret":"smoke-only"}' |
    python3 -c 'import json,sys; print(json.load(sys.stdin).get("token",""))'
}

cargo build -q -p forgevcs-service || { echo "BUILD_FAILED"; exit 1; }
BIN="${CARGO_TARGET_DIR:-$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')}/debug/forgevcs-service"

# --- SHADOW_COMPARE ---------------------------------------------------------------------------
SHADOW_PORT=$PORT
FORGEVCS_AUTH_MODE=SHADOW_COMPARE PORT="$SHADOW_PORT" "$BIN" >/tmp/forgevcs-w6-shadow.log 2>&1 &
SHADOW_PID=$!
wait_health "$SHADOW_PORT" || { echo "shadow did not start"; FAIL=1; }
TOKEN=$(mint "$SHADOW_PORT")
SHADOW_CODE=$(curl -sS -o /tmp/w6-shadow-refs -w '%{http_code}' \
  -H "authorization: Bearer $TOKEN" "http://127.0.0.1:$SHADOW_PORT/repos/forgevcs/refs")
printf '%-14s HTTP %s\n' "shadow-refs" "$SHADOW_CODE"
[ "$SHADOW_CODE" = 200 ] || FAIL=1
kill "$SHADOW_PID" 2>/dev/null
wait "$SHADOW_PID" 2>/dev/null
SHADOW_PID=

# --- DUAL_ENFORCE -----------------------------------------------------------------------------
DUAL_PORT=$((PORT + 1))
FORGEVCS_AUTH_MODE=DUAL_ENFORCE PORT="$DUAL_PORT" "$BIN" >/tmp/forgevcs-w6-dual.log 2>&1 &
DUAL_PID=$!
wait_health "$DUAL_PORT" || { echo "dual did not start"; FAIL=1; }
TOKEN=$(mint "$DUAL_PORT")
DUAL_CODE=$(curl -sS -o /tmp/w6-dual-refs -w '%{http_code}' \
  -H "authorization: Bearer $TOKEN" "http://127.0.0.1:$DUAL_PORT/repos/forgevcs/refs")
printf '%-14s HTTP %s\n' "dual-refs" "$DUAL_CODE"
[ "$DUAL_CODE" = 403 ] || FAIL=1
kill "$DUAL_PID" 2>/dev/null
wait "$DUAL_PID" 2>/dev/null
DUAL_PID=

# --- persisted decisions ----------------------------------------------------------------------
SHADOW_ROW=$(psql "$DB" -tAc "select count(*) from forgevcs_auth_decisions \
  where mode='SHADOW_COMPARE' and action='read' and scion_allow and not culebra_allow \
  and final_allow and mismatch" 2>/dev/null | tr -d '[:space:]')
DUAL_ROW=$(psql "$DB" -tAc "select count(*) from forgevcs_auth_decisions \
  where mode='DUAL_ENFORCE' and action='read' and scion_allow and not culebra_allow \
  and not final_allow" 2>/dev/null | tr -d '[:space:]')
printf '%-14s %s row(s)\n' "shadow-decision" "${SHADOW_ROW:-<query failed>}"
printf '%-14s %s row(s)\n' "dual-decision" "${DUAL_ROW:-<query failed>}"
[ "${SHADOW_ROW:-0}" -ge 1 ] || FAIL=1
[ "${DUAL_ROW:-0}" -ge 1 ] || FAIL=1

# --- invalid mode is not ready ----------------------------------------------------------------
BAD_PORT=$((PORT + 2))
FORGEVCS_AUTH_MODE=NOT_A_MODE PORT="$BAD_PORT" "$BIN" >/tmp/forgevcs-w6-bad.log 2>&1 &
BAD_PID=$!
wait_health "$BAD_PORT" || { echo "bad-mode process did not start"; FAIL=1; }
BAD_READY=$(curl -sS -o /tmp/w6-bad-ready -w '%{http_code}' "http://127.0.0.1:$BAD_PORT/ready")
printf '%-14s HTTP %s\n' "invalid-mode" "$BAD_READY"
[ "$BAD_READY" = 503 ] || FAIL=1
kill "$BAD_PID" 2>/dev/null
wait "$BAD_PID" 2>/dev/null
BAD_PID=

[ "$FAIL" = 0 ] && echo "W6_AUTHZ=PASS" || echo "W6_AUTHZ=FAIL"
exit "$FAIL"
