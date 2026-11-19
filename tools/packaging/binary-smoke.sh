#!/usr/bin/env bash
# Starts `<binary> serve` on free loopback ports with a fresh data directory and runs smoke.sh
# against it, then `cetra health` (D-N-4, D-N-13).
#
# Usage: binary-smoke.sh <path to the cetra binary>
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if [[ $# -ne 1 ]]; then
  echo "usage: binary-smoke.sh <binary>" >&2
  exit 2
fi
bin="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"

# Three ports, held open together so no two are the same, then released for the server.
read -r http_port otlp_port grpc_port < <(python3 -c '
import socket

socks = [socket.socket() for _ in range(3)]
for sock in socks:
    sock.bind(("127.0.0.1", 0))
print(*(sock.getsockname()[1] for sock in socks))
for sock in socks:
    sock.close()
')

dir="$(mktemp -d)"
server_pid=""
cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    for _ in $(seq 20); do
      kill -0 "$server_pid" 2>/dev/null || break
      sleep 0.5
    done
    kill -KILL "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$dir"
}
trap cleanup EXIT

key="sk-cetra-SMOKE0123456789ABCDEFGHIJKLMNOPQ"

CETRA_DATA_DIR="$dir/data" \
CETRA_PUBLIC_URL="http://127.0.0.1:${http_port}" \
CETRA_HTTP_ADDR="127.0.0.1:${http_port}" \
CETRA_OTLP_HTTP_ADDR="127.0.0.1:${otlp_port}" \
CETRA_GRPC_ADDR="127.0.0.1:${grpc_port}" \
CETRA_INIT_ADMIN_EMAIL="smoke@example.com" \
CETRA_INIT_ADMIN_PASSWORD="cetra-smoke-1" \
CETRA_INIT_ORG="Smoke" \
CETRA_INIT_PROJECT="Smoke" \
CETRA_INIT_SECRET_KEY="$key" \
  "$bin" serve >"$dir/serve.log" 2>&1 &
server_pid=$!

if ! "$root/tools/packaging/smoke.sh" "http://127.0.0.1:${http_port}" "http://127.0.0.1:${otlp_port}" "$key"; then
  cat "$dir/serve.log" >&2
  exit 1
fi
"$bin" health --url "http://127.0.0.1:${http_port}"
echo "binary-smoke: $bin passed"
