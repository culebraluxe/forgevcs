#!/usr/bin/env bash
# Print the `FORGEVCS_HUMAN_PRINCIPALS` entry for a human secret.
#
# The secret is read from stdin (never from argv, so it does not land in the shell history or in
# the process list) and only its SHA-256 digest is printed. The service never sees the secret
# itself: it stores only this digest and compares it in constant time when the CLI mints a token.
#
#   printf '%s' "$MY_HUMAN_SECRET" | scripts/human-principal-hash.sh \
#       --login culebraluxe --email culebraluxe@gmail.com --display Culebraluxe
#
# Output (stdout, one line, ready for FORGEVCS_HUMAN_PRINCIPALS):
#   {"culebraluxe":{"email":"culebraluxe@gmail.com","display":"Culebraluxe","secret_sha256":"…"}}
#
# Usage: human-principal-hash.sh [--login LOGIN] [--email EMAIL] [--display NAME]
set -euo pipefail

login="culebraluxe"
email="culebraluxe@gmail.com"
display="Culebraluxe"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --login) [ "$#" -ge 2 ] || { echo "--login needs a value" >&2; exit 2; }; login="$2"; shift 2 ;;
    --email) [ "$#" -ge 2 ] || { echo "--email needs a value" >&2; exit 2; }; email="$2"; shift 2 ;;
    --display) [ "$#" -ge 2 ] || { echo "--display needs a value" >&2; exit 2; }; display="$2"; shift 2 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

# The secret is one line from stdin; do not echo it.
IFS= read -r secret || true
if [ -z "${secret}" ]; then
  echo "error: no secret on stdin" >&2
  exit 2
fi

if command -v shasum >/dev/null 2>&1; then
  digest="$(printf '%s' "$secret" | shasum -a 256 | awk '{print $1}')"
elif command -v sha256sum >/dev/null 2>&1; then
  digest="$(printf '%s' "$secret" | sha256sum | awk '{print $1}')"
else
  echo "error: need shasum or sha256sum to hash the secret" >&2
  exit 2
fi
unset secret

# Keep the value JSON-safe; these are operator-supplied labels, validated by the service too.
json_escape() { printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'; }

printf '{"%s":{"email":"%s","display":"%s","secret_sha256":"%s"}}\n' \
  "$(json_escape "$login")" "$(json_escape "$email")" "$(json_escape "$display")" "$digest"
