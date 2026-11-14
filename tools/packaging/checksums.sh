#!/usr/bin/env bash
# Writes <dir>/SHA256SUMS for every .tar.gz in <dir>, in `sha256sum` format (D-N-7).
#
# Usage: checksums.sh <dir>
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: checksums.sh <dir>" >&2
  exit 2
fi
cd "$1"
shopt -s nullglob
files=(*.tar.gz)
if (( ${#files[@]} == 0 )); then
  echo "checksums.sh: no .tar.gz in $1" >&2
  exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "${files[@]}" >SHA256SUMS
else
  shasum -a 256 "${files[@]}" >SHA256SUMS
fi
echo "wrote $1/SHA256SUMS (${#files[@]} files)"
