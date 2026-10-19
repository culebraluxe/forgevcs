#!/usr/bin/env bash
# Shared helpers for the ForgeVCS backup schedule: PATH hardening, a durable status file, and a
# local-only failure alert (macOS notification + an alert log). No external service.
#
# Sourced by:
#   scripts/scheduled-backup.sh   (daily zip)
#   scripts/restore-drill.sh      (weekly unattended restore drill)
#   scripts/backup-alert-test.sh  (proves a failed run surfaces an alert)
#   scripts/backup-acceptance.sh  (drill boot)
#
# launchd does not inherit a login shell: `cargo`/`python3` from Homebrew or ~/.cargo are NOT on
# its PATH. That is exactly why the installed daily job failed with "cargo: command not found" and
# nobody was told. Every entrypoint calls forgevcs_harden_path before using a tool, and a missing
# tool is a first-class failure the finalizer alerts on.
set -uo pipefail

# Put the operator's real tool directories on PATH regardless of how we were launched, and resolve
# the tools once so a later PATH change cannot silently swap them.
forgevcs_harden_path() {
  local cargo_bin="$HOME/.cargo/bin"
  local candidates="$cargo_bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
  PATH="$candidates:${PATH:-}"
  export PATH
  FORGEVCS_CARGO="${FORGEVCS_CARGO:-$(command -v cargo 2>/dev/null || true)}"
  FORGEVCS_PYTHON3="${FORGEVCS_PYTHON3:-$(command -v python3 2>/dev/null || true)}"
  FORGEVCS_PSQL="${FORGEVCS_PSQL:-$(command -v psql 2>/dev/null || true)}"
  FORGEVCS_CURL="${FORGEVCS_CURL:-$(command -v curl 2>/dev/null || true)}"
  FORGEVCS_OSASCRIPT="${FORGEVCS_OSASCRIPT:-$(command -v osascript 2>/dev/null || true)}"
  export FORGEVCS_CARGO FORGEVCS_PYTHON3 FORGEVCS_PSQL FORGEVCS_CURL FORGEVCS_OSASCRIPT
}

# launchd-safe default. NOT ~/Documents: Homebrew's Python.app is TCC-scoped for Documents, and a
# launchd (no GUI) run blocks forever on the consent prompt instead of failing. ~/Library is not
# TCC-scoped. FORGEVCS_BACKUP_DIR still overrides this; the legacy Documents directory is only read
# for the one-time migration below.
forgevcs_backup_dir() {
  printf '%s\n' "${FORGEVCS_BACKUP_DIR:-$HOME/Library/Application Support/forgevcs-backups}"
}

# The pre-2026-10-19 location. Kept readable for migration; never written by the schedule.
forgevcs_legacy_backup_dir() {
  printf '%s\n' "$HOME/Documents/forgevcs-backups"
}

# One status file per job kind: the weekly drill must never overwrite the daily backup's status.
#   scheduled-backup -> .backup-status.json        (the stable contract docs refer to)
#   restore-drill    -> .restore-drill-status.json
#   anything else    -> .<kind>-status.json
forgevcs_status_file() {
  local kind="${1:-scheduled-backup}"
  case "$kind" in
    scheduled-backup) printf '%s\n' "$(forgevcs_backup_dir)/.backup-status.json" ;;
    restore-drill) printf '%s\n' "$(forgevcs_backup_dir)/.restore-drill-status.json" ;;
    *) printf '%s\n' "$(forgevcs_backup_dir)/.${kind}-status.json" ;;
  esac
}

forgevcs_alerts_log() {
  printf '%s\n' "$(forgevcs_backup_dir)/.backup-alerts.log"
}

forgevcs_summary_file() {
  printf '%s\n' "$(forgevcs_backup_dir)/.last-backup-summary.json"
}

forgevcs_utc_now() {
  date -u +%Y-%m-%dT%H:%M:%SZ
}

# Post a local macOS notification. Best effort: a headless launchd context may have no GUI session,
# in which case the status file and alert log are still written (the durable half of the alert).
# Returns 0 when a notification was accepted, 1 when suppressed or unavailable.
forgevcs_notify() {
  local title="$1"
  local message="$2"
  if [ "${FORGEVCS_BACKUP_ALERTS:-1}" = "0" ]; then
    return 1
  fi
  [ -n "${FORGEVCS_OSASCRIPT:-}" ] || return 1
  local safe_title safe_message
  safe_title=$(printf '%s' "$title" | sed 's/\\/\\\\/g; s/"/\\"/g')
  safe_message=$(printf '%s' "$message" | sed 's/\\/\\\\/g; s/"/\\"/g')
  "$FORGEVCS_OSASCRIPT" \
    -e "display notification \"$safe_message\" with title \"$safe_title\"" \
    >/dev/null 2>&1
}

# Write the status file (JSON when python3 is available, a plain .txt fallback when not) and, on
# failure, alert. Prints an operator digest to stdout. Returns the exit code it recorded.
#
#   forgevcs_finalize <kind> <started_epoch> <rc> <summary> <log> [notify:0|1]
forgevcs_finalize() {
  local kind="$1"
  local started_epoch="$2"
  local rc="$3"
  local summary="$4"
  local log="$5"
  local notify="${6:-1}"

  local dir status_file alerts_log now finished duration notification="none"
  dir=$(forgevcs_backup_dir)
  status_file=$(forgevcs_status_file "$kind")
  alerts_log=$(forgevcs_alerts_log)
  now=$(forgevcs_utc_now)
  finished=$(date +%s)
  duration=$(( finished - started_epoch ))
  mkdir -p "$dir" 2>/dev/null || true

  if [ "$rc" -ne 0 ]; then
    if [ "$notify" = "1" ] && forgevcs_notify "ForgeVCS backup FAILED ($kind)" "exit $rc; status: $status_file"; then
      notification="macos-notification"
    fi
    printf '%s ALERT kind=%s exit=%s status=%s log=%s summary=%s\n' \
      "$now" "$kind" "$rc" "$status_file" "${log:-none}" \
      "$(printf '%s' "$summary" | tail -1)" >>"$alerts_log" 2>/dev/null || true
  fi

  if [ -n "${FORGEVCS_PYTHON3:-}" ] && [ -x "$FORGEVCS_PYTHON3" ]; then
    "$FORGEVCS_PYTHON3" - "$kind" "$rc" "$started_epoch" "$duration" "$now" \
      "$summary" "$log" "$dir" "$status_file" "$notify" "$notification" <<'PY' >/dev/null 2>&1 || true
import json
import os
import socket
import sys

kind, rc, started, duration, finished, summary, log, backup_dir, status_path, notify, notification = sys.argv[1:12]
payload = {
    "format": "forgevcs-backup-status-v1",
    "kind": kind,
    "status": "ok" if int(rc) == 0 else "failed",
    "exit_code": int(rc),
    "started_epoch": int(started),
    "duration_seconds": int(duration),
    "finished_at": finished,
    "backup_dir": backup_dir,
    "log": log or None,
    "host": socket.gethostname(),
    "summary_lines": summary.splitlines()[-20:],
    "alert": {
        "notify_requested": notify == "1",
        "notification": notification if int(rc) != 0 else "none",
        "status_file": status_path,
        "alerts_log": os.path.join(backup_dir, ".backup-alerts.log"),
    },
}
with open(status_path, "w", encoding="utf-8") as handle:
    json.dump(payload, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY
  else
    {
      printf 'format=forgevcs-backup-status-v1\n'
      printf 'kind=%s\nstatus=%s\nexit_code=%s\nfinished_at=%s\n' \
        "$kind" "$([ "$rc" -eq 0 ] && echo ok || echo failed)" "$rc" "$now"
      printf 'duration_seconds=%s\nalert.notification=%s\n' "$duration" "$notification"
      printf 'alert.status_file=%s\n' "$status_file"
    } >"$status_file" 2>/dev/null || true
  fi

  local last_line
  last_line=$(printf '%s' "$summary" | grep -v '^[[:space:]]*$' | tail -1)
  printf 'BACKUP DIGEST kind=%s status=%s exit=%s duration=%ss dir=%s summary=%s\n' \
    "$kind" "$([ "$rc" -eq 0 ] && echo ok || echo failed)" "$rc" "$duration" "$dir" "$last_line"
  printf 'STATUS_FILE=%s\n' "$status_file"
  if [ "$rc" -ne 0 ]; then
    printf 'ALERT_LOG=%s\n' "$alerts_log"
    [ "$notification" = "macos-notification" ] && printf 'ALERT=macos-notification\n'
  fi
  return "$rc"
}

# Preflight the tools the backup path cannot work without, with a message an operator can act on.
forgevcs_require_tools() {
  local missing=()
  [ -n "${FORGEVCS_CARGO:-}" ] || missing+=(cargo)
  [ -n "${FORGEVCS_PYTHON3:-}" ] || missing+=(python3)
  [ -n "${FORGEVCS_CURL:-}" ] || missing+=(curl)
  if [ "${#missing[@]}" -ne 0 ]; then
    echo "FAIL  missing tools on PATH after hardening: ${missing[*]}" >&2
    echo "      launchd does not inherit the login shell; install them or set FORGEVCS_CARGO/" >&2
    echo "      FORGEVCS_PYTHON3 explicitly." >&2
  fi
  # psql is the TD-2 dependency; report it here so the failure is one clear line instead of a
  # mid-zip error.
  if [ -z "${FORGEVCS_PSQL:-}" ]; then
    echo "FAIL  psql not found: the metadata half of the archive is a psql COPY export (TD-2)." >&2
    echo "      brew install postgresql@16  (or set FORGEVCS_PSQL=/path/to/psql)" >&2
    missing+=(psql)
  fi
  [ "${#missing[@]}" -eq 0 ]
}
