#!/usr/bin/env bash
# Mirror (or drift-check) the three CulebraLuxe repos into hosted ForgeVCS.
#
#   bash scripts/mirror-all.sh            # mirror every repo
#   bash scripts/mirror-all.sh --check    # exit 3 if any repo drifted; write nothing
#
# Env:
#   FORGEVCS_BASE_URL       default https://culebraluxe-forgevcs.vercel.app
#   FORGEVCS_SECRET_FILE    default /Users/Shared/dev/.forgevcs-vercel-maestro.secret
#   FORGEVCS_REPOS_ROOT     default /Users/Shared/dev/src
#   FORGEVCS_MAX_PUSH_BYTES default 3000000
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BASE="${FORGEVCS_BASE_URL:-https://culebraluxe-forgevcs.vercel.app}"
SECRET_FILE="${FORGEVCS_SECRET_FILE:-/Users/Shared/dev/.forgevcs-vercel-maestro.secret}"
REPOS_ROOT="${FORGEVCS_REPOS_ROOT:-/Users/Shared/dev/src}"
MAX_BYTES="${FORGEVCS_MAX_PUSH_BYTES:-3000000}"
CHECK=0
[[ "${1:-}" == "--check" ]] && CHECK=1
[[ -f "$SECRET_FILE" ]] || { echo "ERROR: no secret file $SECRET_FILE" >&2; exit 2; }
SECRET="$(cat "$SECRET_FILE")"
BYPASS="$(vercel project protection --json 2>/dev/null |
  python3 -c 'import json,sys; d=json.load(sys.stdin); print(next(iter(d.get("protectionBypass", {})), ""))' 2>/dev/null || true)"
TOKEN="$(curl -sS --max-time 30 -H "x-vercel-protection-bypass: $BYPASS" -X POST "$BASE/auth/token" \
  -H 'content-type: application/json' -d "{\"service\":\"maestro\",\"secret\":\"$SECRET\"}" |
  python3 -c 'import json,sys; print(json.load(sys.stdin).get("token",""))')"
[[ -n "$TOKEN" ]] || { echo "ERROR: could not mint ForgeVCS token" >&2; exit 2; }
PYTHON="${FORGEVCS_PYTHON:-/tmp/forgevcs-venv/bin/python}"
[[ -x "$PYTHON" ]] || PYTHON=python3

# repo name | git checkout | extra mirror args (app excludes local-only binary assets)
REPOS=(
  "culebraluxe-web|$REPOS_ROOT/lane-deep|--exclude public/"
  "forge|$REPOS_ROOT/forge|"
  "workflow|$REPOS_ROOT/workflow|"
)
status=0
for entry in "${REPOS[@]}"; do
  IFS='|' read -r name path extra <<<"$entry"
  [[ -d "$path" ]] || { echo "SKIP $name: $path missing" >&2; status=2; continue; }
  args=(--git-repo "$path" --git-ref HEAD --base "$BASE" --repo "$name" --token "$TOKEN" --bypass "$BYPASS" --max-push-bytes "$MAX_BYTES")
  if [[ -n "$extra" ]]; then read -r -a extra_args <<<"$extra"; args+=("${extra_args[@]}"); fi
  if [[ "$CHECK" == 1 ]]; then args+=(--check); fi
  echo "=== $name ==="
  "$PYTHON" "$ROOT/scripts/forge-mirror.py" "${args[@]}" || {
    rc=$?
    if [[ "$CHECK" == 1 && "$rc" == 3 ]]; then status=3; else status=$rc; fi
  }
done
exit "$status"
