#!/usr/bin/env bash
# W8 acceptance: the hosted ForgeVCS service on Vercel, not a local process.
#
#   scripts/w8-hosted-smoke.sh
#   FORGEVCS_HOSTED_URL=https://… scripts/w8-hosted-smoke.sh
#
# Uses the project's automation-bypass secret to reach a protected deployment, the
# deployment's configured service principal (kept outside the repo at
# /Users/Shared/dev/.forgevcs-vercel-maestro.secret), and the same W4/W5 clients that run
# locally. It creates unique repositories and leaves them in place; V0 has no repository
# deletion endpoint yet.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 2

BASE="${FORGEVCS_HOSTED_URL:-https://culebraluxe-forgevcs.vercel.app}"
SECRET_FILE="${FORGEVCS_DEPLOY_SECRET_FILE:-/Users/Shared/dev/.forgevcs-vercel-maestro.secret}"
FAIL=0

if [ ! -f "$SECRET_FILE" ]; then
  echo "no deployment secret at $SECRET_FILE" >&2
  exit 2
fi
SECRET=$(cat "$SECRET_FILE")
STAMP=$(date +%y%m%d%H%M%S)
W4_REPO="forgevcs-w4-$STAMP"
W5_REPO="forgevcs-w5-$STAMP"

BYPASS=$(vercel project protection --json 2>/dev/null |
  python3 -c 'import json,sys; d=json.load(sys.stdin); print(next(iter(d.get("protectionBypass", {})), ""))' 2>/dev/null || true)
CURL_AUTH=()
[ -n "$BYPASS" ] && CURL_AUTH=(-H "x-vercel-protection-bypass: $BYPASS")

curl_json() {
  curl -sS --max-time 120 "${CURL_AUTH[@]}" "$@"
}

echo "host: $BASE"
for path in /health /ready /version; do
  printf '%-10s ' "$path"
  BODY=$(curl_json -o /tmp/forgevcs-w8-body -w '%{http_code}' "$BASE$path")
  printf 'HTTP %s  ' "$BODY"
  head -c 240 /tmp/forgevcs-w8-body
  echo
  [ "$BODY" = 200 ] || FAIL=1
done

TOKEN=$(curl_json -X POST "$BASE/auth/token" \
  -H 'content-type: application/json' \
  -d "{\"service\":\"maestro\",\"secret\":\"$SECRET\"}" |
  python3 -c 'import json,sys; print(json.load(sys.stdin).get("token",""))' 2>/dev/null || true)
printf '%-10s ' token
if [ -n "$TOKEN" ]; then echo "minted (${#TOKEN} chars)"; else echo "MISSING"; FAIL=1; fi

BYPASS_ARG=()
[ -n "$BYPASS" ] && BYPASS_ARG=(--bypass "$BYPASS")

printf '%-10s ' "W4($W4_REPO)"
if python3 "$ROOT/scripts/w4-acceptance.py" --base "$BASE" --token "$TOKEN" --repo "$W4_REPO" "${BYPASS_ARG[@]}"; then
  echo "W4_HOSTED=PASS"
else
  echo "W4_HOSTED=FAIL"
  FAIL=1
fi

printf '%-10s ' "W5($W5_REPO)"
if python3 "$ROOT/scripts/w5-acceptance.py" --base "$BASE" --token "$TOKEN" --repo "$W5_REPO" "${BYPASS_ARG[@]}"; then
  echo "W5_HOSTED=PASS"
else
  echo "W5_HOSTED=FAIL"
  FAIL=1
fi

[ "$FAIL" = 0 ] && echo "W8_HOSTED=PASS" || echo "W8_HOSTED=FAIL"
exit "$FAIL"
