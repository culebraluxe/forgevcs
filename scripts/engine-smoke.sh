#!/usr/bin/env bash
# Boot the service against the live Neon branch and drive the W4 acceptance path.
#
#   scripts/engine-smoke.sh            # uses .env.local, port 3999
#   PORT=4100 scripts/engine-smoke.sh
#
# Probes, the auth seam, then the business façade on a fresh repo: init → refs → push →
# commits → pull. Exits non-zero if any of those statuses, the landed message or the pulled
# bytes differ. Needs `.env.local` (neon env pull) plus a bucket on the branch; sets nothing
# itself except a throwaway service principal so the engine has a policy identity to boot with.
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
export PORT="${PORT:-3999}"

FAIL=0
BASE="http://127.0.0.1:$PORT"

LOG="${TMPDIR:-/tmp}/forgevcs-smoke-$PORT.log"
cargo build -q -p forgevcs-service || { echo "BUILD_FAILED"; exit 1; }
BIN="${CARGO_TARGET_DIR:-$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')}/debug/forgevcs-service"

"$BIN" >"$LOG" 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null' EXIT

for _ in $(seq 1 30); do
  sleep 1
  curl -fsS "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 && break
done

for path in /health /ready /readyz /version; do
  printf '%-10s ' "$path"
  curl -s -o /tmp/forgevcs-smoke-body -w 'HTTP %{http_code}  ' "http://127.0.0.1:$PORT$path"
  head -c 300 /tmp/forgevcs-smoke-body
  echo
done

# The auth loop (ADR-0002 D4): mint a token from the throwaway principal, spend it on whoami,
# then prove the two failure modes are 401 rather than 500.
TOKEN=$(curl -sS -X POST "http://127.0.0.1:$PORT/auth/token" \
  -H 'content-type: application/json' \
  -d '{"service":"maestro","secret":"smoke-only"}' |
  sed -n 's/.*"token":"\([^"]*\)".*/\1/p')

printf '%-10s ' token
if [ -n "$TOKEN" ]; then echo "minted (${#TOKEN} chars)"; else echo "MISSING"; fi

printf '%-10s ' whoami
curl -s -o /tmp/forgevcs-smoke-body -w 'HTTP %{http_code}  ' \
  -H "authorization: Bearer $TOKEN" "http://127.0.0.1:$PORT/auth/whoami"
head -c 300 /tmp/forgevcs-smoke-body
echo

printf '%-10s ' bad-secret
curl -s -o /tmp/forgevcs-smoke-body -w 'HTTP %{http_code}  ' \
  -X POST "http://127.0.0.1:$PORT/auth/token" \
  -H 'content-type: application/json' \
  -d '{"service":"maestro","secret":"not-the-secret"}'
head -c 300 /tmp/forgevcs-smoke-body
echo

printf '%-10s ' no-token
curl -s -o /tmp/forgevcs-smoke-body -w 'HTTP %{http_code}  ' \
  "http://127.0.0.1:$PORT/auth/whoami"
head -c 300 /tmp/forgevcs-smoke-body
echo

# --- W4 SourceControlService acceptance ------------------------------------------------------
#
# The client is scripts/w4-acceptance.py, so the same test can run against the local process
# here and against the hosted Vercel deployment in W8. It covers init, two changesets (A then a
# modification B), history, B.parent == A, both file versions, the tree, the diff and merge-base.
REPO="forgevcs-smoke"
printf '%-14s ' "W4($REPO)"
if python3 "$ROOT/scripts/w4-acceptance.py" --base "$BASE" --token "$TOKEN" --repo "$REPO"; then
  echo "W4_ACCEPTANCE=PASS"
else
  echo "W4_ACCEPTANCE=FAIL"
  FAIL=1
fi

echo "log: $LOG"
exit "$FAIL"
