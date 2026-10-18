#!/usr/bin/env bash
# Boot a local ForgeVCS engine with the DevOps seat and back up / restore one repository.
#
#   scripts/repo-backup.sh backup  --repo <name> --zip /path/archive.zip
#   scripts/repo-backup.sh restore --repo <name> --zip /path/archive.zip [--replace]
#
# Metadata goes through psql (needs psql locally, owner connection from .env.local); object bytes
# go through the DevOps blob-transfer routes. Restore recreates the same repository name, so the
# deterministic repo_id and the archive's blob keys line up.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 2
[ "$#" -ge 1 ] || { echo "usage: $0 backup|restore --repo <name> --zip <path>" >&2; exit 2; }

if [ ! -f .env.local ]; then
  echo "no .env.local: run 'neon env pull' first" >&2
  exit 2
fi
set -a
# shellcheck disable=SC1091
. ./.env.local
set +a

export FORGEVCS_DATABASE_URL="${DATABASE_URL_UNPOOLED:-$DATABASE_URL}"
export FORGEVCS_BACKUP_DATABASE_URL="$FORGEVCS_DATABASE_URL"
export FORGEVCS_BUCKET="${FORGEVCS_BUCKET:-forgevcs-objects}"
export FORGEVCS_POLICY_PATH="${FORGEVCS_POLICY_PATH:-$ROOT/scripts/pilot/acl-roles.toml}"
export FORGEVCS_AUTH_MODE="${FORGEVCS_AUTH_MODE:-SCION_ONLY}"
export PORT="${PORT:-3994}"

SECRET_DEVOPS="backup-devops-secret"
hash_of() { printf '%s' "$1" | shasum -a 256 | cut -d' ' -f1; }
export FORGEVCS_SERVICE_PRINCIPALS="devops=$(hash_of "$SECRET_DEVOPS")"

BASE="http://127.0.0.1:$PORT"
LOG="${TMPDIR:-/tmp}/forgevcs-backup-$PORT.log"

cargo build -q -p forgevcs-service || { echo "BUILD_FAILED"; exit 1; }
TARGET_DIR="${CARGO_TARGET_DIR:-$(cargo metadata --format-version 1 --no-deps |
  sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')}"
BIN="$TARGET_DIR/debug/forgevcs-service"
"$BIN" >"$LOG" 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null' EXIT

for _ in $(seq 1 60); do
  sleep 1
  curl -fsS "$BASE/health" >/dev/null 2>&1 && break
done
curl -fsS "$BASE/health" >/dev/null 2>&1 || {
  echo "engine did not become healthy on $BASE; log tail:" >&2
  tail -40 "$LOG" >&2
  exit 1
}

python3 "$ROOT/scripts/repo_backup.py" "$1" \
  --base "$BASE" \
  --devops-secret "$SECRET_DEVOPS" \
  "${@:2}"
RC=$?
if [ "$RC" -ne 0 ]; then
  echo "engine log tail ($LOG):" >&2
  tail -40 "$LOG" >&2
fi
exit "$RC"
