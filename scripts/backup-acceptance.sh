#!/usr/bin/env bash
# Boot a local ForgeVCS engine with the DevOps seat and run the backup/restore drill.
#
#   scripts/backup-acceptance.sh
#
# Creates a repository with two changesets, zips it, verifies the archive against its manifest,
# deletes it, restores it (exact row/blob counts, refs and file bytes), then deletes it again and
# runs the orphan GC — the drill leaves nothing behind. JSON evidence is written next to the
# backups (override with FORGEVCS_BACKUP_EVIDENCE).
#
# No production mutation: it uses the configured DATABASE_URL branch and mutates only the
# throwaway drill repository it creates.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 2
# shellcheck source=backup-lib.sh
. "$ROOT/scripts/backup-lib.sh"
forgevcs_harden_path

forgevcs_require_tools || exit 1

if [ ! -f .env.local ]; then
  echo "no .env.local: run 'neon env pull' first" >&2
  exit 2
fi
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
PORT="${PORT:-3994}"
export PORT

EVIDENCE_DIR="$FORGEVCS_BACKUP_DIR/drills"
mkdir -p "$EVIDENCE_DIR" || { echo "cannot create evidence dir $EVIDENCE_DIR" >&2; exit 2; }
EVIDENCE="${FORGEVCS_BACKUP_EVIDENCE:-$EVIDENCE_DIR/acceptance-$(date -u +%Y%m%dT%H%M%SZ).json}"

SECRET_DEVOPS="backup-devops-secret"
hash_of() { printf '%s' "$1" | shasum -a 256 | cut -d' ' -f1; }
SERVICE_HASH=$(hash_of "$SECRET_DEVOPS")
export FORGEVCS_SERVICE_PRINCIPALS="devops=$SERVICE_HASH"

BASE="http://127.0.0.1:$PORT"
LOG="${TMPDIR:-/tmp}/forgevcs-backup-accept-$PORT.log"

BUILD_OUTPUT=$("$FORGEVCS_CARGO" build -q -p forgevcs-service 2>&1 | tail -20)
BUILD_RC=$?
if [ "$BUILD_RC" -ne 0 ]; then
  echo "BUILD_FAILED:" >&2
  printf '%s\n' "$BUILD_OUTPUT" >&2
  exit 1
fi
TARGET_DIR="${CARGO_TARGET_DIR:-$("$FORGEVCS_CARGO" metadata --format-version 1 --no-deps |
  sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')}"
BIN="$TARGET_DIR/debug/forgevcs-service"
[ -x "$BIN" ] || { echo "service binary missing: $BIN" >&2; exit 1; }

"$BIN" >"$LOG" 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null' EXIT

for _ in $(seq 1 60); do
  sleep 1
  "$FORGEVCS_CURL" -fsS "$BASE/health" >/dev/null 2>&1 && break
done
"$FORGEVCS_CURL" -fsS "$BASE/health" >/dev/null 2>&1 || {
  echo "engine did not become healthy on $BASE; log tail:" >&2
  tail -40 "$LOG" >&2
  exit 1
}

cd "$ROOT/scripts" || exit 2
"$FORGEVCS_PYTHON3" backup_acceptance.py \
  --base "$BASE" \
  --devops-secret "$SECRET_DEVOPS" \
  --database-url "$FORGEVCS_BACKUP_DATABASE_URL" \
  --evidence "$EVIDENCE"
RC=$?
if [ "$RC" -ne 0 ]; then
  echo "engine log tail ($LOG):" >&2
  tail -40 "$LOG" >&2
  echo "ACCEPTANCE FAILED (evidence, if written: $EVIDENCE)" >&2
else
  echo "ACCEPTANCE OK  evidence=$EVIDENCE"
fi
exit "$RC"
