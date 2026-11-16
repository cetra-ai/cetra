#!/usr/bin/env bash
# Checks that every file carrying the release version agrees with the workspace version (D-N-5):
# deploy/helm/cetra/Chart.yaml (version, appVersion), the default image of
# deploy/compose/compose.yaml and the commented image of deploy/compose/.env.example.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
version="$(tools/packaging/version.sh)"
status=0

expect() {
  # expect <file> <exact line>
  if ! grep -qxF "$2" "$1"; then
    echo "versions-check: $1 lacks the line: $2" >&2
    status=1
  fi
}

expect deploy/helm/cetra/Chart.yaml "version: $version"
expect deploy/helm/cetra/Chart.yaml "appVersion: \"$version\""
expect deploy/compose/compose.yaml "    image: \${CETRA_IMAGE:-ghcr.io/cetra-ai/cetra:$version}"
expect deploy/compose/.env.example "#CETRA_IMAGE=ghcr.io/cetra-ai/cetra:$version"

if [[ "$status" -eq 0 ]]; then
  echo "versions-check: every file says $version"
fi
exit "$status"
