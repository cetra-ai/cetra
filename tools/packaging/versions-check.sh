#!/usr/bin/env bash
# Checks that every file carrying the release version agrees with the workspace version (D-N-5):
# deploy/helm/cetra/Chart.yaml (version, appVersion), the default image of
# deploy/compose/compose.yaml, the commented image of deploy/compose/.env.example, and the SDK
# versions (D-N-37): sdks/typescript/package.json and VERSION in its src/index.ts, and
# sdks/python/pyproject.toml and its uv.lock, in PEP 440 form (pep440.sh).
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
expect sdks/typescript/package.json "  \"version\": \"$version\","
expect sdks/typescript/src/index.ts "export const VERSION = \"$version\";"
if py_version="$(tools/packaging/pep440.sh "$version")"; then
  expect sdks/python/pyproject.toml "version = \"$py_version\""
  locked="$(awk '/^name = "cetra-sdk"$/ { getline; print; exit }' sdks/python/uv.lock)"
  if [[ "$locked" != "version = \"$py_version\"" ]]; then
    echo "versions-check: sdks/python/uv.lock has cetra-sdk $locked, not $py_version" >&2
    status=1
  fi
else
  status=1
fi

if [[ "$status" -eq 0 ]]; then
  echo "versions-check: every file says $version"
fi
exit "$status"
