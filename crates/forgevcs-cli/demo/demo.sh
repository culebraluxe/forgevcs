#!/usr/bin/env bash
# Human-path demo for the `forgevcs` CLI.
#
# This script exists so the acceptance flow from docs/WORK-ORDER-HUMAN-CLI.md (§3) is one
# runnable sequence instead of a paragraph: health → auth status → init → refs → log → push a
# changeset → ref create (lane ref at the pushed changeset) → refs/log again → pull the object
# back → workspace create/status/destroy.
#
# Prerequisites:
#   * a reachable ForgeVCS service (`FORGEVCS_BASE_URL`, default http://127.0.0.1:3000);
#   * a minted token:  forgevcs auth mint --login culebraluxe
#     (or pass --token-file / --config-dir below);
#   * jq and openssl (only this demo script needs them, never the CLI).
#
# Usage:
#   FORGEVCS_BASE_URL=https://… ./demo.sh
#   FORGEVCS_BIN=/path/to/forgevcs FORGEVCS_DEMO_REPO=my-demo ./demo.sh
#
# Optional: DEMO_A and DEMO_B are two divergent changeset ids. When both are set, the script
# also demonstrates merge / rebase / merge-land; a single linear repository cannot produce two
# divergent changesets on its own without a second writer or a lane ref.
#
# No Git wire protocol, no deploy, no publish. Nothing here runs automatically in CI.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASIC_FIXTURE="$SCRIPT_DIR/push-body.json"

FORGEVCS_BIN="${FORGEVCS_BIN:-forgevcs}"
FORGEVCS_BASE_URL="${FORGEVCS_BASE_URL:-http://127.0.0.1:3000}"
FORGEVCS_CONFIG_DIR="${FORGEVCS_CONFIG_DIR:-$HOME/.config/forgevcs}"
REPO="${FORGEVCS_DEMO_REPO:-human-cli-demo}"
REF="${FORGEVCS_DEMO_REF:-main}"
LANE_REF="${FORGEVCS_DEMO_LANE:-demo/lane}"
BLOB_ID="b18d4a0cdcc288bea07a73911afc5ec5eefa1a31ad3a8bcc190d7deb50ba2bb298"

command -v jq >/dev/null || { echo "demo needs jq" >&2; exit 1; }
command -v openssl >/dev/null || { echo "demo needs openssl" >&2; exit 1; }
command -v "$FORGEVCS_BIN" >/dev/null || {
  echo "cannot find the CLI ($FORGEVCS_BIN); build it with: cargo build -p forgevcs-cli" >&2
  exit 1
}

CLI=("$FORGEVCS_BIN" --base-url "$FORGEVCS_BASE_URL" --config-dir "$FORGEVCS_CONFIG_DIR")

step() { printf '\n== %s ==\n' "$1"; }
# Trace to stderr so `... | jq` in the script sees only the command's JSON on stdout.
run() { printf '+' >&2; printf ' %q' "$@" >&2; printf '\n' >&2; "$@"; }

step "health — /health, /version, /ready"
run "${CLI[@]}" health

step "auth status — local token state + the service's verified identity"
run "${CLI[@]}" auth status
if ! "${CLI[@]}" auth status | jq -e '.token_present == true' >/dev/null; then
  cat >&2 <<EOF
No bearer token is stored in $FORGEVCS_CONFIG_DIR.
Mint one first (the human secret is read from stdin, --secret-file or FORGEVCS_HUMAN_SECRET):

  printf '%s' "\$FORGEVCS_HUMAN_SECRET" | $FORGEVCS_BIN auth mint --login culebraluxe
EOF
  exit 1
fi

step "init $REPO — create/claim the repository (idempotent)"
run "${CLI[@]}" init "$REPO"

step "refs — $REF target and version"
TIP="$("${CLI[@]}" refs "$REPO" | jq -r --arg ref "$REF" '.refs[] | select(.name == $ref) | .target')"
[[ -n "$TIP" && "$TIP" != "null" ]] || { echo "no $REF ref after init" >&2; exit 1; }
echo "tip: $TIP"

step "log — first page of history"
run "${CLI[@]}" log "$REPO" --ref "$REF" --limit 5

step "push — upload one blob and a changeset that lands on $REF"
BODY="$(mktemp -t forgevcs-demo-push.XXXXXX.json)"
trap 'rm -f "$BODY"' EXIT
CHANGE_ID="$(openssl rand -hex 16)"
jq --arg parent "$TIP" --arg change "$CHANGE_ID" --arg ref "$REF" '
  .changeset.parents = [$parent]
  | .changeset.change_id = $change
  | .changeset.meta.author_time = { seconds: (now | floor), tz_offset_minutes: 0 }
  | .ref = $ref
' "$BASIC_FIXTURE" > "$BODY"
PUSHED="$(run "${CLI[@]}" push "$REPO" --changeset "$BODY" --ref "$REF" --mode land | jq -r '.changeset.id')"
echo "pushed changeset: $PUSHED"

step "ref create — provision lane ref $LANE_REF at the pushed changeset (needs admin.refs)"
if "${CLI[@]}" refs "$REPO" | jq -e --arg name "$LANE_REF" '.refs[] | select(.name == $name)' >/dev/null; then
  echo "$LANE_REF already exists (idempotent demo re-run)"
else
  run "${CLI[@]}" ref create "$REPO" --name "$LANE_REF" --target "$PUSHED" --reason "human CLI demo"
fi
run "${CLI[@]}" refs "$REPO"

step "log — the new changeset is now on $REF"
run "${CLI[@]}" log "$REPO" --ref "$REF" --limit 5

step "pull — fetch the pushed blob's bytes back"
run "${CLI[@]}" pull "$REPO" --ref "$REF" --objects "$BLOB_ID"

step "workspace create/status/destroy"
WORKSPACE_ID="$("${CLI[@]}" workspace create "$REPO" --ref "$REF" | jq -r '.workspace_id')"
echo "workspace: $WORKSPACE_ID"
run "${CLI[@]}" workspace status "$REPO" "$WORKSPACE_ID"
run "${CLI[@]}" workspace destroy "$REPO" "$WORKSPACE_ID"

if [[ -n "${DEMO_A:-}" && -n "${DEMO_B:-}" ]]; then
  step "merge / rebase / merge-land — two divergent changesets (DEMO_A, DEMO_B)"
  run "${CLI[@]}" merge "$REPO" --a "$DEMO_A" --b "$DEMO_B" \
    --change-id "$(openssl rand -hex 16)" --message "demo merge"
  run "${CLI[@]}" rebase "$REPO" --changeset "$DEMO_A" --onto "$DEMO_B"
  run "${CLI[@]}" merge-land "$REPO" --head "$DEMO_B" --ref "$REF" --message "demo merge-land"
else
  step "merge / rebase / merge-land — skipped"
  cat <<'EOF'
These take two divergent changeset ids, which a single linear demo cannot create. When a
second writer (or a lane ref) exists, run them explicitly, e.g.:

  forgevcs merge      REPO --a <CHANGESET_A> --b <CHANGESET_B> --change-id "$(openssl rand -hex 16)"
  forgevcs rebase     REPO --changeset <CHANGESET_A> --onto <CHANGESET_B>
  forgevcs merge-land REPO --head <CHANGESET_B> --ref main --message "merge-land demo"

Or re-run with DEMO_A=<id> DEMO_B=<id>.
EOF
fi

step "done"
echo "No Git wire protocol was used; no deploy or publish happened."
