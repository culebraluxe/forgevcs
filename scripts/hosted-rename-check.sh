#!/usr/bin/env bash
# Prove the exact-rename engine is active on the hosted deployment.
#
#   scripts/hosted-rename-check.sh
#
# Needs the deployment's automation-bypass secret (Vercel project protection) and the maestro
# service secret, both kept outside the repo.
set -uo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 2
BASE="${FORGEVCS_HOSTED_URL:-https://culebraluxe-forgevcs.vercel.app}"
SECRET_FILE="${FORGEVCS_DEPLOY_SECRET_FILE:-/Users/Shared/dev/.forgevcs-vercel-maestro.secret}"
[ -f "$SECRET_FILE" ] || { echo "no deployment secret at $SECRET_FILE" >&2; exit 2; }
SECRET=$(cat "$SECRET_FILE")
BYPASS=$(npx --yes vercel@64.5.0 project protection --json 2>/dev/null |
  python3 -c 'import json,sys; d=json.load(sys.stdin); print(next(iter(d.get("protectionBypass", {})), ""))' 2>/dev/null || true)
[ -n "$BYPASS" ] || { echo "could not read the automation-bypass secret from Vercel" >&2; exit 2; }
cd "$ROOT/scripts"
python3 hosted_rename_check.py --base "$BASE" --secret "$SECRET" --bypass "$BYPASS"
