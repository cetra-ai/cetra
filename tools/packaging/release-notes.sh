#!/usr/bin/env bash
# Prints the body of CHANGELOG.md's `## [<version>] - <date>` section (the GitHub release notes).
# Fails when the section is missing or empty (D-N-5).
#
# Usage: release-notes.sh <version>
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if [[ $# -ne 1 ]]; then
  echo "usage: release-notes.sh <version>" >&2
  exit 2
fi
notes="$(awk -v v="$1" '
  index($0, "## [" v "] - ") == 1 { on = 1; next }
  /^## \[/ { on = 0 }
  on && (NF || started) { started = 1; print }
' "$root/CHANGELOG.md")"
if [[ -z "${notes//[[:space:]]/}" ]]; then
  echo "release-notes: CHANGELOG.md has no section '## [$1] - <date>' with content" >&2
  exit 1
fi
printf '%s\n' "$notes"
