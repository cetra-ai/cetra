#!/usr/bin/env bash
# Prints the workspace version, `version` in `[workspace.package]` of the root Cargo.toml (D-N-5).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
version="$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version = "\(.*\)"$/\1/p;}' "$root/Cargo.toml")"
if [[ -z "$version" ]]; then
  echo "version.sh: no version in [workspace.package] of Cargo.toml" >&2
  exit 1
fi
echo "$version"
