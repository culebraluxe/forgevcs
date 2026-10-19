#!/usr/bin/env bash
# Daily backup job (launchd target): archive every repository to the Mac backup directory, verify
# each zip, apply the retention window, and make a failure visible.
#
#   scripts/scheduled-backup.sh
#
# Offsite standard (owner decision, 2026-10-18): the Mac-local zip IS the accepted offsite copy;
# Neon keeps its own database backups. This job therefore does not ship archives to object storage.
#
# Outputs:
#   $FORGEVCS_BACKUP_DIR/<repo>/<UTC stamp>.zip   one verified archive per repository
#   $FORGEVCS_BACKUP_DIR/.backup-status.json      durable status: ok/failed, exit code, digest
#   $FORGEVCS_BACKUP_DIR/.last-backup-summary.json machine-readable run summary
#   $FORGEVCS_BACKUP_DIR/.backup-alerts.log       append-only alert audit
#   a local macOS notification on failure (best effort; the status file is the durable alert)
#
# Exit is non-zero on any failure, so launchd's log also records it.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 2
# shellcheck source=backup-lib.sh
. "$ROOT/scripts/backup-lib.sh"
forgevcs_harden_path

STARTED=$(date +%s)
LOG="${TMPDIR:-/tmp}/forgevcs-scheduled-backup.log"
SUMMARY=""
FINISHED=0
ENGINE_PID=""

finish() {
  local rc="$1"
  FINISHED=1
  stop_watchdog
  [ -n "$ENGINE_PID" ] && kill "$ENGINE_PID" 2>/dev/null
  forgevcs_finalize "scheduled-backup" "$STARTED" "$rc" "$SUMMARY" "$LOG" 1
  exit "$rc"
}

fail() {
  SUMMARY="${SUMMARY}${SUMMARY:+$'\n'}FAIL  $1"
  finish 1
}

# If anything exits unexpectedly (a `set -u` error, a signal), still record and alert instead of
# dying silently in a launchd log nobody reads.
on_exit() {
  local rc=$?
  [ -n "$ENGINE_PID" ] && kill "$ENGINE_PID" 2>/dev/null
  stop_watchdog
  if [ "$FINISHED" -eq 0 ]; then
    SUMMARY="${SUMMARY}${SUMMARY:+$'\n'}FAIL  unexpected exit (rc=$rc); see $LOG"
    FINISHED=1
    forgevcs_finalize "scheduled-backup" "$STARTED" 1 "$SUMMARY" "$LOG" 1
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
  forgevcs_finalize "scheduled-backup" "$STARTED" 1 "$SUMMARY" "$LOG" 1
  exit 143
}
trap on_signal INT TERM
start_watchdog "${BACKUP_JOB_TIMEOUT_SECONDS:-600}"

forgevcs_require_tools || fail "backup preflight failed (see tool messages above)"

[ -f .env.local ] || fail "no .env.local (run 'neon env pull' and add DATABASE_URL_UNPOOLED)"
# shellcheck disable=SC1091
set -a
. ./.env.local
set +a

export FORGEVCS_DATABASE_URL="${DATABASE_URL_UNPOOLED:-$DATABASE_URL}"
export FORGEVCS_BACKUP_DATABASE_URL="$FORGEVCS_DATABASE_URL"
export FORGEVCS_BUCKET="${FORGEVCS_BUCKET:-forgevcs-objects}"
export FORGEVCS_POLICY_PATH="${FORGEVCS_POLICY_PATH:-$ROOT/scripts/pilot/acl-roles.toml}"
export FORGEVCS_AUTH_MODE="${FORGEVCS_AUTH_MODE:-SCION_ONLY}"
export PORT="${PORT:-3993}"
FORGEVCS_BACKUP_DIR="$(forgevcs_backup_dir)"
export FORGEVCS_BACKUP_DIR
export BACKUP_RETENTION_DAYS="${BACKUP_RETENTION_DAYS:-7}"

case "$BACKUP_RETENTION_DAYS" in
  '' | *[!0-9]*) fail "BACKUP_RETENTION_DAYS must be a positive integer, got '$BACKUP_RETENTION_DAYS'" ;;
esac
[ "$BACKUP_RETENTION_DAYS" -ge 1 ] || fail "BACKUP_RETENTION_DAYS must be >= 1"
[ "$BACKUP_RETENTION_DAYS" -le 365 ] || fail "BACKUP_RETENTION_DAYS=$BACKUP_RETENTION_DAYS is implausible (cap 365)"

[ -n "${FORGEVCS_BACKUP_DATABASE_URL:-}" ] || fail "no database URL (DATABASE_URL_UNPOOLED/DATABASE_URL in .env.local)"
mkdir -p "$FORGEVCS_BACKUP_DIR" || fail "cannot create backup dir $FORGEVCS_BACKUP_DIR"

SECRET_DEVOPS="backup-devops-secret"
hash_of() { printf '%s' "$1" | shasum -a 256 | cut -d' ' -f1; }
SERVICE_HASH=$(hash_of "$SECRET_DEVOPS")
export FORGEVCS_SERVICE_PRINCIPALS="devops=$SERVICE_HASH"
BASE="http://127.0.0.1:$PORT"

{
  echo "=== scheduled backup $(forgevcs_utc_now) host=$(hostname) ==="
  echo "cargo=$FORGEVCS_CARGO python3=$FORGEVCS_PYTHON3 psql=$FORGEVCS_PSQL"
  echo "backup_dir=$FORGEVCS_BACKUP_DIR retention_days=$BACKUP_RETENTION_DAYS"
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

BACKUP_OUTPUT=$(cd "$ROOT/scripts" && "$FORGEVCS_PYTHON3" backup_all.py \
  --base "$BASE" \
  --devops-secret "$SECRET_DEVOPS" \
  --database-url "$FORGEVCS_BACKUP_DATABASE_URL" \
  --dir "$FORGEVCS_BACKUP_DIR" \
  --retention-days "$BACKUP_RETENTION_DAYS" \
  --summary "$(forgevcs_summary_file)" 2>&1)
BACKUP_RC=$?
SUMMARY="$BACKUP_OUTPUT"
if [ "$BACKUP_RC" -ne 0 ]; then
  { echo "--- backup_all.py failed; engine log tail ---"; tail -40 "$LOG"; } >&2
fi
finish "$BACKUP_RC"
