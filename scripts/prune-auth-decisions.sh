#!/usr/bin/env bash
# R6 — prune old `forgevcs_auth_decisions` rows (dry run by default).
#
#   scripts/prune-auth-decisions.sh                      # dry run, 90-day window
#   scripts/prune-auth-decisions.sh --days 30 --json
#   scripts/prune-auth-decisions.sh --apply --confirm    # delete (explicit, batched)
#
# Loads the same Neon branch credentials as the other maintenance scripts (.env.local from
# `neon env pull`). No migration runner, no service restart: the table is created at boot and
# the prune is a batched DELETE on the `at` index. Run the dry run first.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 2

if [ -f .env.local ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env.local
  set +a
fi

export FORGEVCS_DATABASE_URL="${FORGEVCS_DATABASE_URL:-${DATABASE_URL_UNPOOLED:-$DATABASE_URL}}"
[ -n "$FORGEVCS_DATABASE_URL" ] || {
  echo "no database URL: set FORGEVCS_DATABASE_URL (or .env.local via 'neon env pull')" >&2
  exit 2
}

exec python3 "$ROOT/scripts/prune-auth-decisions.py" "$@"
