#!/usr/bin/env bash
# Install (or remove) the ForgeVCS backup launchd jobs:
#
#   com.culebraluxe.forgevcs.backup        daily 02:00  scripts/scheduled-backup.sh
#   com.culebraluxe.forgevcs.restore-drill weekly Sun 03:00  scripts/restore-drill.sh
#
#   scripts/install-backup-schedule.sh             install + load both
#   scripts/install-backup-schedule.sh --no-drill  install only the daily zip
#   scripts/install-backup-schedule.sh --remove    unload + delete both
#
# The plist carries an explicit PATH because launchd does not inherit the login shell; that missing
# PATH is why the previous installed job failed with "cargo: command not found" and nobody was told.
# Both entrypoints also harden their own PATH, so an older installed plist still works.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
LABEL="com.culebraluxe.forgevcs.backup"
DRILL_LABEL="com.culebraluxe.forgevcs.restore-drill"
AGENTS="$HOME/Library/LaunchAgents"
PLIST="$AGENTS/$LABEL.plist"
DRILL_PLIST="$AGENTS/$DRILL_LABEL.plist"
LOGDIR="$HOME/Library/Logs"
# shellcheck source=backup-lib.sh
. "$ROOT/scripts/backup-lib.sh"
forgevcs_harden_path
BACKUP_DIR="$(forgevcs_backup_dir)"
LEGACY_DIR="$(forgevcs_legacy_backup_dir)"
PATH_VALUE="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"

mkdir -p "$AGENTS" "$LOGDIR"

# One-time, non-destructive migration from the pre-launchd-safe ~/Documents location. Homebrew's
# Python.app cannot read ~/Documents from a launchd agent without a GUI TCC prompt (it blocks), so
# the schedule now uses ~/Library/Application Support. The legacy directory is left untouched.
if [ -d "$LEGACY_DIR" ] && [ ! -e "$BACKUP_DIR" ]; then
  mkdir -p "$(dirname "$BACKUP_DIR")"
  cp -R "$LEGACY_DIR" "$BACKUP_DIR"
  echo "migrated existing archives: $LEGACY_DIR -> $BACKUP_DIR (legacy copy left in place)"
fi

remove_job() {
  local label="$1"
  local plist="$2"
  launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || launchctl unload "$plist" 2>/dev/null || true
  rm -f "$plist"
}

if [ "${1:-}" = "--remove" ]; then
  remove_job "$DRILL_LABEL" "$DRILL_PLIST"
  remove_job "$LABEL" "$PLIST"
  echo "removed $LABEL and $DRILL_LABEL"
  exit 0
fi
INSTALL_DRILL=1
if [ "${1:-}" = "--no-drill" ]; then
  INSTALL_DRILL=0
elif [ -n "${1:-}" ]; then
  echo "usage: $0 [--no-drill | --remove]" >&2
  exit 2
fi

write_plist() {
  local label="$1"
  local script="$2"
  local plist="$3"
  local out_log="$4"
  local err_log="$5"
  local calendar="$6"
  cat >"$plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/bash</string>
    <string>$ROOT/scripts/$script</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>$PATH_VALUE</string>
    <key>HOME</key><string>$HOME</string>
  </dict>
  <key>StartCalendarInterval</key>
  <dict>$calendar</dict>
  <key>RunAtLoad</key><false/>
  <key>StandardOutPath</key><string>$out_log</string>
  <key>StandardErrorPath</key><string>$err_log</string>
</dict>
</plist>
PLIST
}

# Boot out any previously loaded job BEFORE writing the new plist, otherwise bootstrap refuses
# the label and the fresh plist is never loaded.
remove_job "$LABEL" "$PLIST"
write_plist "$LABEL" "scheduled-backup.sh" "$PLIST" \
  "$LOGDIR/forgevcs-backup.log" "$LOGDIR/forgevcs-backup.err.log" \
  "<key>Hour</key><integer>2</integer><key>Minute</key><integer>0</integer>"
if ! launchctl bootstrap "gui/$(id -u)" "$PLIST"; then
  echo "FAILED to load $PLIST" >&2
  exit 1
fi
echo "installed $LABEL (daily 02:00) -> $PLIST"

if [ "$INSTALL_DRILL" -eq 1 ]; then
  remove_job "$DRILL_LABEL" "$DRILL_PLIST"
  write_plist "$DRILL_LABEL" "restore-drill.sh" "$DRILL_PLIST" \
    "$LOGDIR/forgevcs-restore-drill.log" "$LOGDIR/forgevcs-restore-drill.err.log" \
    "<key>Weekday</key><integer>0</integer><key>Hour</key><integer>3</integer><key>Minute</key><integer>0</integer>"
  if ! launchctl bootstrap "gui/$(id -u)" "$DRILL_PLIST"; then
    echo "FAILED to load $DRILL_PLIST" >&2
    exit 1
  fi
  echo "installed $DRILL_LABEL (weekly Sunday 03:00) -> $DRILL_PLIST"
fi

echo "launchd does not inherit the login shell; the plists carry PATH explicitly and both scripts"
echo "also harden PATH. Verify with: launchctl list | grep culebraluxe.forgevcs"
echo "status:  $BACKUP_DIR/.backup-status.json"
echo "alerts:  $BACKUP_DIR/.backup-alerts.log"
echo "logs:    $LOGDIR/forgevcs-backup.log $LOGDIR/forgevcs-restore-drill.log"
