#!/usr/bin/env bash
# The release workflow's first step (D-N-5): the tag is v<workspace version>, every versioned file
# agrees, CHANGELOG.md has the section, and the tagged commit is on origin/main.
#
# Usage: release-verify.sh <tag>, e.g. v0.1.0. Needs `git fetch origin main` first.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
if [[ $# -ne 1 ]]; then
  echo "usage: release-verify.sh <tag>" >&2
  exit 2
fi
tag="$1"
version="$(tools/packaging/version.sh)"
if [[ "$tag" != "v$version" ]]; then
  echo "release-verify: tag $tag does not match the workspace version $version" >&2
  exit 1
fi
tools/packaging/versions-check.sh
tools/packaging/release-notes.sh "$version" >/dev/null
# A stable release has its golden data directory, last in RELEASES (spec 03 § 12, D-R-30), and its
# golden server backup (D-R-38).
if [[ "$version" != *-* ]]; then
  golden=fixtures/storage/golden
  if [[ ! -f "$golden/$version/expected.json" ]]; then
    echo "release-verify: $golden/$version/expected.json is missing: run just golden-dir $version" >&2
    exit 1
  fi
  if [[ "$(grep -v '^#' "$golden/RELEASES" | grep . | tail -n 1)" != "$version" ]]; then
    echo "release-verify: $golden/RELEASES does not end with $version" >&2
    exit 1
  fi
  # And its golden server backup, written by the same recipe (spec 04 § Format versions, D-R-38).
  server_golden=fixtures/server/golden
  if [[ ! -f "$server_golden/$version/expected.json" ]]; then
    echo "release-verify: $server_golden/$version/expected.json is missing: run just golden-dir $version" >&2
    exit 1
  fi
fi
if ! git merge-base --is-ancestor HEAD origin/main; then
  echo "release-verify: $tag is not on origin/main" >&2
  exit 1
fi
echo "release-verify: $tag is ready"
