#!/usr/bin/env bash
# ForgeVCS gate runner: the vendored upstream gate, run so that a long build cannot turn
# into a polling loop.
#
#   scripts/gate.sh            # T-core: upstream/scripts/check.sh
#   scripts/gate.sh --full     # T-core + the spec-sized full_* tests (red upstream at the
#                              # baseline SHA; see docs/BASELINE.md before gating on it)
#
# Everything goes to $FORGEVCS_LOG_DIR (default ~/Library/Logs/forgevcs):
#
#   gate-<timestamp>.log   the whole run, last line `GATE_EXIT=<code>`
#   latest.log             symlink to the newest run
#   gate.pid               taken while a run is live
#
# So the check is one command, run whenever convenient — never a wait loop:
#
#   tail -3 ~/Library/Logs/forgevcs/latest.log
#
# Set CARGO_TARGET_DIR to keep the build tree out of the repo; the script prints whichever
# target dir it used. The toolchain comes from upstream/rust-toolchain.toml, not from here.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LOG_DIR=${FORGEVCS_LOG_DIR:-$HOME/Library/Logs/forgevcs}
LOCK="$LOG_DIR/gate.pid"
mkdir -p "$LOG_DIR"

# One gate at a time. Two concurrent runs fight over the cargo build lock and the second
# stalls at "Blocking waiting for file lock" until the first exits, which reads as a hang.
if [ -f "$LOCK" ] && kill -0 "$(cat "$LOCK")" 2>/dev/null; then
  echo "gate already running (pid $(cat "$LOCK")); see $LOG_DIR/latest.log" >&2
  exit 3
fi
echo $$ >"$LOCK"
trap 'rm -f "$LOCK"' EXIT

STAMP=$(date +%Y%m%d-%H%M%S)
LOG="$LOG_DIR/gate-$STAMP.log"
: >"$LOG"
ln -sfn "$LOG" "$LOG_DIR/latest.log"

# Provenance: a green gate is only evidence for the tree it actually ran against, so record
# the commit and whether the tree was dirty at start.
DIRTY=$(git -C "$ROOT" status --porcelain | wc -l | tr -d ' ')
{
  echo "== repo       $ROOT"
  echo "== commit     $(git -C "$ROOT" rev-parse --short HEAD) on $(git -C "$ROOT" rev-parse --abbrev-ref HEAD)"
  echo "== dirty      $DIRTY uncommitted path(s)"
  echo "== target dir ${CARGO_TARGET_DIR:-<cargo config default>}"
  echo "== started    $(date -u +%FT%TZ)"
} >>"$LOG"

cd "$ROOT/upstream" || exit 2
bash scripts/check.sh "$@" >>"$LOG" 2>&1
STATUS=$?

{
  echo "== finished   $(date -u +%FT%TZ)"
  echo "GATE_EXIT=$STATUS"
} >>"$LOG"

echo "log:        $LOG"
echo "GATE_EXIT=$STATUS"
exit "$STATUS"
