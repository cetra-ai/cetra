#!/usr/bin/env bash
# Runs deploy/compose with an image and runs smoke.sh against it (D-N-9, D-N-13). With `s3`,
# adds compose.s3.yaml and checks that a flush wrote objects to the Garage bucket.
#
# Usage: compose-smoke.sh <image> [s3]
# Works on a copy of deploy/compose in a temporary directory, so the checkout stays clean.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if [[ $# -lt 1 || $# -gt 2 || ( $# -eq 2 && "$2" != "s3" ) ]]; then
  echo "usage: compose-smoke.sh <image> [s3]" >&2
  exit 2
fi
image="$1"
mode="${2:-local}"
project="llmobs-smoke-$$"
key="sk-llmobs-SMOKE0123456789ABCDEFGHIJKLMNOPQ"

dir="$(mktemp -d)"
cp -R "$root/deploy/compose/." "$dir/"
files=(-f compose.yaml)
if [[ "$mode" == "s3" ]]; then
  files+=(-f compose.s3.yaml)
fi
compose() {
  (cd "$dir" && docker compose -p "$project" "${files[@]}" "$@")
}
cleanup() {
  compose down -v --remove-orphans >/dev/null 2>&1 || true
  rm -rf "$dir"
}
trap cleanup EXIT

read -r http_port grpc_port otlp_port < <(python3 -c '
import socket

socks = [socket.socket() for _ in range(3)]
for sock in socks:
    sock.bind(("127.0.0.1", 0))
print(*(sock.getsockname()[1] for sock in socks))
for sock in socks:
    sock.close()
')

cat >"$dir/.env" <<EOF
LLMOBS_IMAGE=$image
LLMOBS_BIND=127.0.0.1
LLMOBS_HTTP_PORT=$http_port
LLMOBS_OTLP_GRPC_PORT=$grpc_port
LLMOBS_OTLP_HTTP_PORT=$otlp_port
GARAGE_RPC_SECRET=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
GARAGE_ADMIN_TOKEN=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
GARAGE_ACCESS_KEY_ID=GK$(python3 -c 'import secrets; print(secrets.token_hex(12))')
GARAGE_SECRET_ACCESS_KEY=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
EOF
cat >"$dir/llmobs.env" <<EOF
LLMOBS_PUBLIC_URL=http://127.0.0.1:$http_port
LLMOBS_INIT_ADMIN_EMAIL=smoke@example.com
LLMOBS_INIT_ADMIN_PASSWORD=llmobs-smoke-1
LLMOBS_INIT_ORG=Smoke
LLMOBS_INIT_PROJECT=Smoke
LLMOBS_INIT_SECRET_KEY=$key
LLMOBS_STORAGE__FLUSH__MAX_AGE=5s
EOF

# Not `--wait`: garage-init is a one-shot container. smoke.sh waits for /readyz instead.
if ! compose up -d; then
  compose ps -a >&2
  compose logs >&2
  exit 1
fi
if ! "$root/tools/packaging/smoke.sh" "http://127.0.0.1:$http_port" "http://127.0.0.1:$otlp_port" "$key"; then
  compose logs >&2
  exit 1
fi

if [[ "$mode" == "s3" ]]; then
  # The span is flushed to Parquet in the bucket within flush.max_age (5 s here).
  # shellcheck disable=SC2016 # expanded by the container's shell
  check='curl -sSf -H "Authorization: Bearer $GARAGE_ADMIN_TOKEN" "http://garage:3903/v2/GetBucketInfo?globalAlias=llmobs" | tr -d " \n" | grep -q "\"objects\":[1-9]"'
  for _ in $(seq 1 60); do
    if compose run --rm --no-deps --entrypoint /bin/sh garage-init -c "$check" >/dev/null 2>&1; then
      echo "compose-smoke: objects written to the Garage bucket"
      # garage-init must succeed again on an initialized Garage (a restart, a second `up`).
      if ! compose run --rm --no-deps garage-init; then
        compose logs >&2
        echo "compose-smoke: garage-init failed on its second run" >&2
        exit 1
      fi
      echo "compose-smoke: garage-init is idempotent"
      echo "compose-smoke: $image passed ($mode)"
      exit 0
    fi
    sleep 1
  done
  compose logs >&2
  echo "compose-smoke: no object in the Garage bucket after 60 s" >&2
  exit 1
fi
echo "compose-smoke: $image passed ($mode)"
