#!/usr/bin/env bash
# Prove the failure-alert path end to end WITHOUT running a real backup.
#
#   scripts/backup-alert-test.sh [--notify]
#
# Writes a status file and an alert-log entry for a simulated backup failure under
# $FORGEVCS_ALERT_TEST_DIR (default <backup dir>/alert-test) and, with --notify, posts the same
# local macOS notification a real failure would post. Exits non-zero if the alert artifacts are
# missing. This is acceptance evidence for "a failed backup produces an alert", not a failure
# injection into the real schedule.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 2
# shellcheck source=backup-lib.sh
. "$ROOT/scripts/backup-lib.sh"
forgevcs_harden_path

NOTIFY=0
case "${1:-}" in
  "") ;;
  --notify) NOTIFY=1 ;;
  *)
    echo "usage: $0 [--notify]" >&2
    exit 2
    ;;
esac

BASE_DIR=$(forgevcs_backup_dir)
TARGET_DIR="${FORGEVCS_ALERT_TEST_DIR:-$BASE_DIR/alert-test}"
mkdir -p "$TARGET_DIR" || { echo "FAIL  cannot create $TARGET_DIR" >&2; exit 1; }
export FORGEVCS_BACKUP_DIR="$TARGET_DIR"

STARTED=$(date +%s)
SUMMARY="SIMULATED FAILURE: backup-alert acceptance test ($(forgevcs_utc_now)); no real backup failed"
forgevcs_finalize "alert-test" "$STARTED" 1 "$SUMMARY" "" "$NOTIFY" >/dev/null
FINALIZE_RC=$?
STATUS_FILE="$TARGET_DIR/.alert-test-status.json"
ALERTS_LOG="$TARGET_DIR/.backup-alerts.log"

echo "alert-test dir: $TARGET_DIR"
if [ -f "$STATUS_FILE" ]; then
  echo "status file: $STATUS_FILE"
  "$FORGEVCS_PYTHON3" - "$STATUS_FILE" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    status = json.load(handle)
print(json.dumps(status, indent=2, sort_keys=True))
PY
fi
if [ -f "$ALERTS_LOG" ]; then
  echo "alert log tail:"
  tail -3 "$ALERTS_LOG"
fi

FAILED=0
[ -f "$STATUS_FILE" ] || { echo "FAIL  status file was not written" >&2; FAILED=1; }
if [ -f "$STATUS_FILE" ]; then
  "$FORGEVCS_PYTHON3" - "$STATUS_FILE" <<'PY' || FAILED=1
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    status = json.load(handle)
assert status["status"] == "failed", status
assert status["exit_code"] != 0, status
assert status["kind"] == "alert-test", status
print("PASS  status file records a failed alert-test run")
PY
fi
[ -f "$ALERTS_LOG" ] || { echo "FAIL  alert log was not written" >&2; FAILED=1; }
if [ -f "$ALERTS_LOG" ]; then
  grep -q "ALERT kind=alert-test" "$ALERTS_LOG" \
    && echo "PASS  alert log records the failure" \
    || { echo "FAIL  alert log has no alert line" >&2; FAILED=1; }
fi
if [ "$NOTIFY" -eq 1 ] && [ -f "$STATUS_FILE" ]; then
  NOTIFICATION=$("$FORGEVCS_PYTHON3" - "$STATUS_FILE" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    status = json.load(handle)
print(status["alert"]["notification"])
PY
)
  echo "notification: $NOTIFICATION"
  [ "$NOTIFICATION" = "macos-notification" ] \
    && echo "PASS  macOS notification accepted (see Notification Center)" \
    || { echo "WARN  macOS notification not accepted in this context (status file + alert log are the durable alert)" >&2; }
fi

# finalize intentionally reported failure; this test's own exit reflects the alert artifacts.
[ "$FINALIZE_RC" -ne 0 ] || { echo "FAIL  finalize did not report failure" >&2; FAILED=1; }
[ "$FAILED" -eq 0 ] && echo "OK  failure alert path verified" || echo "FAIL  failure alert path incomplete" >&2
exit "$FAILED"
