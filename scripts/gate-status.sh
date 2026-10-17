#!/usr/bin/env bash
# "Where does the gate stand?" in one read — no log watching, no polling loop.
#
#   scripts/gate-status.sh     exit 0 = last run passed, 1 = failed, 2 = still running,
#                              3 = no run recorded yet
#
# This exists because state must survive a lost session: a fresh agent (or a human) runs this
# once instead of tailing ~/Library/Logs/forgevcs, and knows the commit, the test counts and
# the verdict immediately. It reads only; it never starts or stops a run.
set -uo pipefail

LOG_DIR=${FORGEVCS_LOG_DIR:-$HOME/Library/Logs/forgevcs}
LOG=$(ls -t "$LOG_DIR"/gate-*.log 2>/dev/null | head -1)

if [ -z "$LOG" ]; then
  printf '{"status":"none","log":null}\n'
  exit 3
fi

commit=$(awk -F'  +' '/^== commit/ {print $2; exit}' "$LOG")
dirty=$(awk '/^== dirty/ {print $3; exit}' "$LOG")
target=$(awk '/^== target dir/ {sub(/^== target dir +/, ""); print; exit}' "$LOG")
started=$(awk '/^== started/ {print $3; exit}' "$LOG")
passed=$(awk '/^test result:/ {for (i = 1; i <= NF; i++) if ($i == "passed;") s += $(i - 1)} END {print s + 0}' "$LOG")
failed=$(awk '/^test result:/ {for (i = 1; i <= NF; i++) if ($i == "failed;") s += $(i - 1)} END {print s + 0}' "$LOG")

if grep -q '^GATE_EXIT=0$' "$LOG"; then
  status=pass code=0
elif grep -q '^GATE_EXIT=' "$LOG"; then
  status=fail code=1
else
  status=running code=2
fi

printf '{"status":"%s","commit":"%s","dirty":%s,"target_dir":"%s","started":"%s","tests_passed":%s,"tests_failed":%s,"log":"%s"}\n' \
  "$status" "${commit:-unknown}" "${dirty:-0}" "${target:-unknown}" "${started:-unknown}" \
  "$passed" "$failed" "$LOG"
printf '%s: %s passed, %s failed (%s) — %s\n' \
  "$status" "$passed" "$failed" "${commit:-unknown}" "$LOG" >&2
exit "$code"
