#!/usr/bin/env bash
# Unattended restore drill (launchd target, weekly): boot a local engine against the configured
# control plane, run the backup/restore drill in a throwaway repository, write evidence, and alert
# when it fails.
#
#   scripts/restore-drill.sh
#
# The drill itself (scripts/backup_acceptance.py) creates a repository with history, zips it,
# verifies the archive against its manifest, deletes the repository, restores it, compares row
# counts and blob counts to the archive exactly, proves refs and file bytes survived, then deletes
# its own repository and runs the orphan GC. It never touches an existing repository and never
# mutates production data: it uses the same DATABASE_URL branch as the scheduled backup.
#
# Evidence lands in $FORGEVCS_BACKUP_DIR/drills/restore-drill-<UTC>.json; status/alert use the same
# status file and alert log as the daily backup.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 2
# shellcheck source=backup-lib.sh
. "$ROOT/scripts/backup-lib.sh"
forgevcs_harden_path

STARTED=$(date +%s)
LOG="${TMPDIR:-/tmp}/forgevcs-restore-drill.log"
SUMMARY=""
FINISHED=0
ENGINE_PID=""

finish() {
  local rc="$1"
  FINISHED=1
  stop_watchdog
  [ -n "$ENGINE_PID" ] && kill "$ENGINE_PID" 2>/dev/null
  forgevcs_finalize "restore-drill" "$STARTED" "$rc" "$SUMMARY" "$LOG" 1
  exit "$rc"
}

fail() {
  SUMMARY="${SUMMARY}${SUMMARY:+$'\n'}FAIL  $1"
  finish 1
}

on_exit() {
  local rc=$?
  [ -n "$ENGINE_PID" ] && kill "$ENGINE_PID" 2>/dev/null
  stop_watchdog
  if [ "$FINISHED" -eq 0 ]; then
    SUMMARY="${SUMMARY}${SUMMARY:+$'\n'}FAIL  unexpected exit (rc=$rc); see $LOG"
    FINISHED=1
    forgevcs_finalize "restore-drill" "$STARTED" 1 "$SUMMARY" "$LOG" 1
  fi
}
trap on_exit EXIT

# A hung backup must not hang forever: launchd will happily leave one instance running and skip the
# next day's start. The watchdog sends TERM to this script; the TERM trap finalizes a failure and
# alerts, so a hang becomes visible instead of silent.
WATCHDOG_PID=""
start_watchdog() {
  local limit="$1"
  ( sleep "$limit"; kill -TERM "$$" 2>/dev/null ) &
  WATCHDOG_PID=$!
}
stop_watchdog() {
  if [ -n "$WATCHDOG_PID" ]; then
    kill "$WATCHDOG_PID" 2>/dev/null
    WATCHDOG_PID=""
  fi
}

on_signal() {
  stop_watchdog
  SUMMARY="${SUMMARY}${SUMMARY:+$'\n'}FAIL  interrupted by signal (watchdog timeout or TERM)"
  FINISHED=1
  [ -n "$ENGINE_PID" ] && kill "$ENGINE_PID" 2>/dev/null
  forgevcs_finalize "restore-drill" "$STARTED" 1 "$SUMMARY" "$LOG" 1
  exit 143
}
trap on_signal INT TERM
start_watchdog "${RESTORE_DRILL_TIMEOUT_SECONDS:-900}"

forgevcs_require_tools || fail "restore-drill preflight failed (see tool messages above)"
[ -f .env.local ] || fail "no .env.local (run 'neon env pull')"
# shellcheck disable=SC1091
set -a
. ./.env.local
set +a

export FORGEVCS_DATABASE_URL="${DATABASE_URL_UNPOOLED:-$DATABASE_URL}"
export FORGEVCS_BACKUP_DATABASE_URL="$FORGEVCS_DATABASE_URL"
export FORGEVCS_BUCKET="${FORGEVCS_BUCKET:-forgevcs-objects}"
export FORGEVCS_POLICY_PATH="${FORGEVCS_POLICY_PATH:-$ROOT/scripts/pilot/acl-roles.toml}"
export FORGEVCS_AUTH_MODE="${FORGEVCS_AUTH_MODE:-SCION_ONLY}"
FORGEVCS_BACKUP_DIR="$(forgevcs_backup_dir)"
export FORGEVCS_BACKUP_DIR
PORT="${FORGEVCS_DRILL_PORT:-3995}"
export PORT

[ -n "${FORGEVCS_BACKUP_DATABASE_URL:-}" ] || fail "no database URL (DATABASE_URL_UNPOOLED/DATABASE_URL in .env.local)"
EVIDENCE_DIR="$FORGEVCS_BACKUP_DIR/drills"
mkdir -p "$EVIDENCE_DIR" || fail "cannot create evidence dir $EVIDENCE_DIR"
EVIDENCE="$EVIDENCE_DIR/restore-drill-$(date -u +%Y%m%dT%H%M%SZ).json"
DRILL_ZIP="$EVIDENCE_DIR/.restore-drill.zip"

SECRET_DEVOPS="backup-devops-secret"
hash_of() { printf '%s' "$1" | shasum -a 256 | cut -d' ' -f1; }
SERVICE_HASH=$(hash_of "$SECRET_DEVOPS")
export FORGEVCS_SERVICE_PRINCIPALS="devops=$SERVICE_HASH"
BASE="http://127.0.0.1:$PORT"

{
  echo "=== restore drill $(forgevcs_utc_now) host=$(hostname) ==="
  echo "cargo=$FORGEVCS_CARGO python3=$FORGEVCS_PYTHON3 psql=$FORGEVCS_PSQL port=$PORT"
  echo "evidence=$EVIDENCE"
} >"$LOG" 2>&1

BUILD_OUTPUT=$("$FORGEVCS_CARGO" build -q -p forgevcs-service 2>&1 | tail -20)
BUILD_RC=$?
SUMMARY="$BUILD_OUTPUT"
[ "$BUILD_RC" -eq 0 ] || fail "BUILD_FAILED: $(printf '%s' "$BUILD_OUTPUT" | tail -3)"

TARGET_DIR="${CARGO_TARGET_DIR:-$("$FORGEVCS_CARGO" metadata --format-version 1 --no-deps |
  sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')}"
BIN="$TARGET_DIR/debug/forgevcs-service"
[ -x "$BIN" ] || fail "service binary missing after build: $BIN"

"$BIN" >>"$LOG" 2>&1 &
ENGINE_PID=$!
for _ in $(seq 1 60); do
  sleep 1
  "$FORGEVCS_CURL" -fsS "$BASE/health" >/dev/null 2>&1 && break
done
"$FORGEVCS_CURL" -fsS "$BASE/health" >/dev/null 2>&1 || {
  { echo "engine did not become healthy on $BASE"; tail -40 "$LOG"; } >&2
  fail "engine did not become healthy on $BASE"
}

DRILL_OUTPUT=$(cd "$ROOT/scripts" && "$FORGEVCS_PYTHON3" backup_acceptance.py \
  --base "$BASE" \
  --devops-secret "$SECRET_DEVOPS" \
  --database-url "$FORGEVCS_BACKUP_DATABASE_URL" \
  --zip "$DRILL_ZIP" \
  --evidence "$EVIDENCE" 2>&1)
DRILL_RC=$?
SUMMARY="$DRILL_OUTPUT"
if [ "$DRILL_RC" -ne 0 ]; then
  { echo "--- restore drill failed; engine log tail ---"; tail -40 "$LOG"; } >&2
fi
finish "$DRILL_RC"
