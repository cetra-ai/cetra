#!/usr/bin/env bash
# Starts a real `cetra serve` and runs the Python and TypeScript SDK suites against it (D-G-20).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"

cargo build -p cetra-cli

# Three ports, held open together so no two are the same. They are released before the
# server binds them: another process may take one, and the readiness wait then fails.
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
  if [[ -n "${server_pid}" ]]; then
    kill "${server_pid}" 2>/dev/null || true
    # A drain that hangs must not hold the job until its timeout.
    for _ in $(seq 20); do
      kill -0 "${server_pid}" 2>/dev/null || break
      sleep 0.5
    done
    kill -KILL "${server_pid}" 2>/dev/null || true
    wait "${server_pid}" 2>/dev/null || true
  fi
  rm -rf "$dir"
}
trap cleanup EXIT

key="sk-cetra-0123456789ABCDEFGHIJKLMNOPQRSTUV"
base="http://127.0.0.1:${http_port}"

CETRA_TELEMETRY=off \
CETRA_DATA_DIR="$dir/data" \
CETRA_HTTP_ADDR="127.0.0.1:${http_port}" \
CETRA_PUBLIC_URL="$base" \
CETRA_OTLP_HTTP_ADDR="127.0.0.1:${otlp_port}" \
CETRA_GRPC_ADDR="127.0.0.1:${grpc_port}" \
CETRA_INIT_ADMIN_EMAIL="ada@example.com" \
CETRA_INIT_ADMIN_PASSWORD="cetra-e2e-1" \
CETRA_INIT_ORG="Acme" \
CETRA_INIT_PROJECT="E2E" \
CETRA_INIT_SECRET_KEY="$key" \
  "$root/target/debug/cetra" serve >"$dir/serve.log" 2>&1 &
server_pid=$!

ready=0
for _ in $(seq 1 120); do
  if curl -sf "$base/readyz" >/dev/null; then
    ready=1
    break
  fi
  if ! kill -0 "$server_pid" 2>/dev/null; then
    break
  fi
  sleep 0.5
done
if [[ "$ready" -ne 1 ]]; then
  cat "$dir/serve.log"
  echo "cetra serve did not become ready at $base" >&2
  exit 1
fi

export CETRA_E2E_BASE_URL="$base"
export CETRA_E2E_API_KEY="$key"
uv run --locked --directory sdks/python pytest -m e2e tests/test_e2e.py
pnpm --dir sdks/typescript exec vitest run src/e2e.test.ts
export CETRA_E2E_ADMIN_EMAIL=ada@example.com CETRA_E2E_ADMIN_PASSWORD=cetra-e2e-1
pnpm --dir node-cli run build
pnpm --dir node-cli exec vitest run src/e2e.test.ts
