#!/usr/bin/env bash
# WIRE-1 local harness: git main -> ForgeVCS mirror (read-only, no authority change).
#
# Proves scripts/forge-mirror.py against a REAL local service:
#   local PostgreSQL 16 (already running) + moto S3 (Python venv) + forgevcs-service
#   -> init a fresh repo, mirror a fixture git commit (+ modes/symlink/binary),
#   -> re-run unchanged = NOOP, mutate the tree, mirror again,
#   -> report JSON + per-path sha256 verification.
#
# Requirements: local PG16 on 127.0.0.1:5432, `git`, Python 3, Rust toolchain.
# The venv (moto/blake3/boto3) is created on first run; delete $WIRE1_VENV to refresh.
# No production/Neon writes; cleanup drops only the throwaway database created here.
#
#   scripts/wire1-mirror-harness.sh
#   WIRE1_KEEP=1 scripts/wire1-mirror-harness.sh   # keep DB/logs for inspection
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
VENV=${WIRE1_VENV:-/tmp/forgevcs-wire1-venv}
PORT=${WIRE1_PORT:-3189}
MOTO_PORT=${WIRE1_MOTO_PORT:-5055}
PGHOST=${PGHOST:-127.0.0.1}
PGPORT=${PGPORT:-5432}
PGUSER=${PGUSER:-$(whoami)}
DB="forgevcs_wire1_$(date +%s)_$$"
REPO="wire1-mirror-$$"
SECRET="wire1-secret-$$"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/wire1.XXXXXX")
BUCKET=forgevcs-wire1
SERVICE_PID=""
MOTO_PID=""

log() { printf '%s\n' "$*"; }
fail() { printf 'FAIL  %s\n' "$*" >&2; exit 1; }

cleanup() {
  status=$?
  [ -n "$SERVICE_PID" ] && kill "$SERVICE_PID" 2>/dev/null || true
  [ -n "$MOTO_PID" ] && kill "$MOTO_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  if [ "${WIRE1_KEEP:-0}" != "1" ]; then
    dropdb -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" --if-exists "$DB" >/dev/null 2>&1 || true
  else
    log "kept: db=$DB work=$WORK"
  fi
  exit "$status"
}
trap cleanup EXIT INT TERM

command -v git >/dev/null || fail "git is required"
command -v psql >/dev/null || fail "psql is required"
command -v cargo >/dev/null || fail "cargo is required"
pg_isready -h "$PGHOST" -p "$PGPORT" >/dev/null || fail "local PostgreSQL is not accepting connections at $PGHOST:$PGPORT"

if [ ! -x "$VENV/bin/moto_server" ] || ! "$VENV/bin/python" -c 'import blake3, boto3' >/dev/null 2>&1; then
  log "creating harness venv at $VENV"
  python3 -m venv "$VENV"
  "$VENV/bin/pip" install --quiet "moto[server]" blake3 boto3
fi

# The converter must match a known ForgeVCS blob id before the service is even started:
# `w4-acceptance.py` pins BLOB_A for this exact payload, so this is a fixed external oracle.
"$VENV/bin/python" - <<'PYBLOB'
from blake3 import blake3
known_a = "b1d3bae0cd810e6f0ca2bdfa4a431f5810e4c1878c0ada9ffcddc706c25020d430"
assert "b1" + blake3(b"ForgeSC smoke test\n").hexdigest() == known_a
print(f"PASS  converter matches known ForgeVCS blob id {known_a[:14]} (w4-acceptance BLOB_A)")
PYBLOB

log "== WIRE-1 harness"
log "work=$WORK db=$DB repo=$REPO service=http://127.0.0.1:$PORT moto=http://127.0.0.1:$MOTO_PORT"

psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -tAc "SELECT 1" >/dev/null || fail "cannot connect to PostgreSQL as $PGUSER"
createdb -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" "$DB" || fail "cannot create database $DB"

"$VENV/bin/moto_server" -H 127.0.0.1 -p "$MOTO_PORT" >"$WORK/moto.log" 2>&1 &
MOTO_PID=$!
for _ in $(seq 1 60); do
  curl -fsS "http://127.0.0.1:$MOTO_PORT/moto-api/" >/dev/null 2>&1 && break
  sleep 0.25
done
"$VENV/bin/python" - "$MOTO_PORT" "$BUCKET" <<'PY'
import sys, boto3
port, bucket = sys.argv[1], sys.argv[2]
s3 = boto3.client("s3", endpoint_url=f"http://127.0.0.1:{port}", region_name="us-east-1",
                  aws_access_key_id="test", aws_secret_access_key="test")
s3.create_bucket(Bucket=bucket)
PY

log "building forgevcs-service"
(cd "$ROOT" && cargo build -q -p forgevcs-service)
# Respect the user's cargo target-dir (this workspace uses a shared target dir).
TARGET_DIR=$(cd "$ROOT" && cargo metadata --format-version 1 --no-deps \
  | "$VENV/bin/python" -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')
SERVICE_BIN="${CARGO_TARGET_DIR:-$TARGET_DIR}/debug/forgevcs-service"
[ -x "$SERVICE_BIN" ] || fail "service binary not found at $SERVICE_BIN"

# sha256 of the secret, exactly the digest the service compares.
SECRET_DIGEST=$(printf '%s' "$SECRET" | shasum -a 256 | cut -d' ' -f1)
(
  cd "$ROOT"
  exec env \
    PORT="$PORT" \
    FORGEVCS_DATABASE_URL="postgres://$PGUSER@$PGHOST:$PGPORT/$DB" \
    FORGEVCS_BUCKET="$BUCKET" \
    AWS_REGION="us-east-1" \
    AWS_ENDPOINT_URL_S3="http://127.0.0.1:$MOTO_PORT" \
    AWS_ACCESS_KEY_ID="test" \
    AWS_SECRET_ACCESS_KEY="test" \
    FORGEVCS_SERVICE_PRINCIPALS="maestro=$SECRET_DIGEST" \
    FORGEVCS_POLICY_PATH="$ROOT/acl.toml" \
    FORGEVCS_REPO="$REPO" \
    RUST_LOG=warn \
    "$SERVICE_BIN"
) >"$WORK/service.log" 2>&1 &
SERVICE_PID=$!
for _ in $(seq 1 240); do
  if ! kill -0 "$SERVICE_PID" 2>/dev/null; then
    tail -40 "$WORK/service.log" >&2 || true
    fail "forgevcs-service exited during startup"
  fi
  code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/ready" || true)
  [ "$code" = "200" ] && break
  sleep 0.25
done
code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/ready" || true)
[ "$code" = "200" ] || { tail -40 "$WORK/service.log" >&2 || true; fail "/ready stayed $code"; }

"$VENV/bin/python" - "$PORT" "$REPO" "$SECRET" "$WORK" <<'PY'
import json, sys, urllib.request
port, repo, secret, work = sys.argv[1:5]
base = f"http://127.0.0.1:{port}"
token = None
def call(path, body=None, expect=200, auth=True):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, data=data, method="POST" if data is not None else "GET")
    if data is not None: req.add_header("content-type", "application/json")
    if auth and token: req.add_header("authorization", "Bearer " + token)
    with urllib.request.urlopen(req, timeout=60) as response:
        assert response.status == expect, (path, response.status)
        return json.loads(response.read() or b"{}")
token = call("/auth/token", {"service": "maestro", "secret": secret}, auth=False)["token"]
call(f"/repos/{repo}/init", {})
open(f"{work}/token", "w").write(token)
print("token minted and repo initialized")
PY

TOKEN=$(cat "$WORK/token")
GIT="$WORK/fixture"
git init -q -b main "$GIT"
git -C "$GIT" config user.name "WIRE-1 Fixture"
git -C "$GIT" config user.email "wire1@example.test"
mkdir -p "$GIT/src" "$GIT/docs" "$GIT/bin" "$GIT/assets"
printf 'fixture readme v1\n' >"$GIT/README.md"
printf 'fn main() { println!("v1"); }\n' >"$GIT/src/main.rs"
printf 'guide v1\n' >"$GIT/docs/guide.md"
printf '#!/bin/sh\necho wire1\n' >"$GIT/bin/run.sh"
chmod +x "$GIT/bin/run.sh"
printf 'binary\000payload\377\n' >"$GIT/assets/data.bin"
ln -s README.md "$GIT/link.txt"
git -C "$GIT" add -A
git -C "$GIT" commit -qm "fixture v1"

MIRROR=("$VENV/bin/python" "$ROOT/scripts/forge-mirror.py" --git-repo "$GIT" --git-ref main
        --base "http://127.0.0.1:$PORT" --repo "$REPO" --token "$TOKEN")

expect_report() {
  local report=$1 mode=$2 verified=$3
  "$VENV/bin/python" - "$report" "$mode" "$verified" <<'PY'
import json, sys
report, mode, verified = sys.argv[1:4]
data = json.load(open(report))
assert data["mode"] == mode, f"mode {data['mode']} != {mode}"
assert data["verified"] is (verified == "true"), f"verified {data['verified']}"
assert data["counts"]["added"] >= 0 and data["counts"]["deleted"] >= 0
assert data["checked_paths"] == data["git_paths"], (data["checked_paths"], data["git_paths"])
assert data["tree_id_reproduced"] is True, (data.get("root_tree_id"), data.get("server_root_tree_id"))
print(f"PASS  report {report}: mode={mode} target={data['target']} verified sha256={data['checked_paths']} tree_id={data.get('root_tree_id')}")
PY
}

log "== mirror 1 (initial tree)"
"${MIRROR[@]}" --json-report "$WORK/report-1.json" || fail "first mirror failed"
expect_report "$WORK/report-1.json" mirrored true
TARGET_1=$("$VENV/bin/python" -c "import json;print(json.load(open('$WORK/report-1.json'))['target'])")

log "== mirror 2 (unchanged tree -> NOOP)"
"${MIRROR[@]}" --json-report "$WORK/report-2.json" || fail "NOOP mirror failed"
expect_report "$WORK/report-2.json" noop true
TARGET_2=$("$VENV/bin/python" -c "import json;print(json.load(open('$WORK/report-2.json'))['target'])")
[ "$TARGET_1" = "$TARGET_2" ] || fail "NOOP moved the ref: $TARGET_1 -> $TARGET_2"

log "== mutate fixture (modify/add/delete/mode change) and mirror 3"
printf 'fixture readme v2\n' >"$GIT/README.md"
printf 'fn added() {}\n' >"$GIT/src/added.rs"
git -C "$GIT" rm -q docs/guide.md
chmod +x "$GIT/src/main.rs"
git -C "$GIT" add -A
git -C "$GIT" commit -qm "fixture v2"
"${MIRROR[@]}" --json-report "$WORK/report-3.json" || fail "delta mirror failed"
expect_report "$WORK/report-3.json" mirrored true
"$VENV/bin/python" - "$WORK/report-3.json" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
assert data["counts"] == {"added": 1, "modified": 2, "deleted": 1, "unchanged": 3}, data["counts"]
assert data["target"] != data["base_target"]
print(f"PASS  delta counts {data['counts']} base={data['base_target']} target={data['target']}")
PY

log "== mirror 4 (unchanged after delta -> NOOP)"
"${MIRROR[@]}" --json-report "$WORK/report-4.json" || fail "second NOOP failed"
expect_report "$WORK/report-4.json" noop true

log "WIRE-1 PASS"
log "artifacts: $WORK (reports, token, service.log, moto.log)"
if [ "${WIRE1_KEEP:-0}" = "1" ]; then log "kept database $DB and work dir"; fi
