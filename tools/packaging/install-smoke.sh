#!/usr/bin/env bash
# Serves target/dist over HTTP on loopback and runs deploy/install.sh against it, then checks
# the installed binary's version (D-N-15). Needs `just dist <target>` for this host first.
#
# Usage: install-smoke.sh
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
version="$(tools/packaging/version.sh)"

dir="$(mktemp -d)"
server_pid=""
cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$dir"
}
trap cleanup EXIT

mkdir -p "$dir/www" "$dir/bin"
shopt -s nullglob
archives=(target/dist/cetra-"$version"-*.tar.gz)
if (( ${#archives[@]} == 0 )); then
  echo "install-smoke: no target/dist/cetra-$version-*.tar.gz; run just dist <target> first" >&2
  exit 1
fi
cp "${archives[@]}" "$dir/www/"
tools/packaging/checksums.sh "$dir/www"

port="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$dir/www" >/dev/null 2>&1 &
server_pid=$!
for _ in $(seq 1 50); do
  curl -sf "http://127.0.0.1:$port/SHA256SUMS" >/dev/null && break
  sleep 0.1
done
if ! curl -sf "http://127.0.0.1:$port/SHA256SUMS" >/dev/null; then
  echo "install-smoke: the local HTTP server did not start on port $port" >&2
  exit 1
fi

CETRA_VERSION="$version" \
CETRA_INSTALL_DIR="$dir/bin" \
CETRA_DOWNLOAD_BASE="http://127.0.0.1:$port" \
  sh deploy/install.sh

"$dir/bin/cetra" version | grep -qx "cetra $version"
echo "install-smoke: install.sh installed cetra $version"
