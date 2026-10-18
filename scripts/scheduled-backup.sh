#!/usr/bin/env bash
# Daily backup job (launchd target): archive every repository, keep the retention window.
#
#   scripts/scheduled-backup.sh
#
# Archives land in $FORGEVCS_BACKUP_DIR (default ~/Documents/forgevcs-backups) with
# BACKUP_RETENTION_DAYS (default 7) per repository. Logs go to the launchd plist paths.
set -uo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 2
[ -f .env.local ] || { echo "no .env.local" >&2; exit 2; }
set -a
# shellcheck disable=SC1091
. ./.env.local
set +a

export FORGEVCS_DATABASE_URL="${DATABASE_URL_UNPOOLED:-$DATABASE_URL}"
export FORGEVCS_BACKUP_DATABASE_URL="$FORGEVCS_DATABASE_URL"
export FORGEVCS_BUCKET="${FORGEVCS_BUCKET:-forgevcs-objects}"
export FORGEVCS_POLICY_PATH="${FORGEVCS_POLICY_PATH:-$ROOT/scripts/pilot/acl-roles.toml}"
export FORGEVCS_AUTH_MODE="${FORGEVCS_AUTH_MODE:-SCION_ONLY}"
export PORT="${PORT:-3993}"
export FORGEVCS_BACKUP_DIR="${FORGEVCS_BACKUP_DIR:-$HOME/Documents/forgevcs-backups}"
export BACKUP_RETENTION_DAYS="${BACKUP_RETENTION_DAYS:-7}"

SECRET_DEVOPS="backup-devops-secret"
hash_of() { printf '%s' "$1" | shasum -a 256 | cut -d' ' -f1; }
export FORGEVCS_SERVICE_PRINCIPALS="devops=$(hash_of "$SECRET_DEVOPS")"
BASE="http://127.0.0.1:$PORT"
LOG="${TMPDIR:-/tmp}/forgevcs-scheduled-backup-$PORT.log"

cargo build -q -p forgevcs-service || { echo "BUILD_FAILED"; exit 1; }
TARGET_DIR="${CARGO_TARGET_DIR:-$(cargo metadata --format-version 1 --no-deps |
  sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')}"
"$TARGET_DIR/debug/forgevcs-service" >"$LOG" 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null' EXIT
for _ in $(seq 1 60); do
  sleep 1
  curl -fsS "$BASE/health" >/dev/null 2>&1 && break
done
curl -fsS "$BASE/health" >/dev/null 2>&1 || {
  echo "engine did not become healthy on $BASE" >&2
  tail -40 "$LOG" >&2
  exit 1
}
cd "$ROOT/scripts"
python3 backup_all.py \
  --base "$BASE" \
  --devops-secret "$SECRET_DEVOPS" \
  --database-url "$FORGEVCS_BACKUP_DATABASE_URL" \
  --dir "$FORGEVCS_BACKUP_DIR" \
  --retention-days "$BACKUP_RETENTION_DAYS"
RC=$?
[ "$RC" -ne 0 ] && { echo "engine log tail ($LOG):" >&2; tail -40 "$LOG" >&2; }
exit "$RC"
