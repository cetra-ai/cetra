#!/usr/bin/env bash
# Prepares release <version> (D-N-5): sets the workspace version, Cargo.lock, the chart's version
# and appVersion and the compose image, and turns CHANGELOG.md's Unreleased section into
# `## [<version>] - <today, UTC>` under a new empty Unreleased section. Commit the result, merge
# it to main, then tag that commit `v<version>`.
#
# Usage: release-prep.sh <version>, e.g. 0.1.0 or 0.1.0-rc.1
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"

if [[ $# -ne 1 ]]; then
  echo "usage: release-prep.sh <version>" >&2
  exit 2
fi
new="$1"
if ! [[ "$new" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "release-prep: '$new' is not a SemVer version without build metadata" >&2
  exit 1
fi
old="$(tools/packaging/version.sh)"
if [[ "$new" == "$old" ]]; then
  echo "release-prep: the workspace is already at $new" >&2
  exit 1
fi
if grep -qF "## [$new]" CHANGELOG.md; then
  echo "release-prep: CHANGELOG.md already has a section for $new" >&2
  exit 1
fi

# The Unreleased section must say something.
unreleased="$(awk '/^## \[Unreleased\]$/ { on = 1; next } /^## \[/ { on = 0 } on && NF' CHANGELOG.md)"
if [[ -z "$unreleased" ]]; then
  echo "release-prep: CHANGELOG.md's [Unreleased] section is empty" >&2
  exit 1
fi

sed -i.bak "/^\[workspace\.package\]/,/^\[/{s/^version = \".*\"$/version = \"$new\"/;}" Cargo.toml
sed -i.bak -e "s/^version: .*/version: $new/" -e "s/^appVersion: .*/appVersion: \"$new\"/" \
  deploy/helm/cetra/Chart.yaml
sed -i.bak "s|ghcr.io/cetra-ai/cetra:$old}|ghcr.io/cetra-ai/cetra:$new}|" deploy/compose/compose.yaml
sed -i.bak "s|^#CETRA_IMAGE=ghcr.io/cetra-ai/cetra:$old$|#CETRA_IMAGE=ghcr.io/cetra-ai/cetra:$new|" \
  deploy/compose/.env.example
today="$(date -u +%Y-%m-%d)"
sed -i.bak "s/^## \[Unreleased\]$/## [Unreleased]\n\n## [$new] - $today/" CHANGELOG.md
rm -f Cargo.toml.bak deploy/helm/cetra/Chart.yaml.bak deploy/compose/compose.yaml.bak \
  deploy/compose/.env.example.bak CHANGELOG.md.bak

cargo update --workspace --offline
tools/packaging/versions-check.sh
# A stable release writes its golden data directory with this build (spec 03 § 12, D-R-30);
# a pre-release gets none.
if [[ "$new" != *-* ]]; then
  just golden-dir "$new"
fi
echo "release-prep: $old -> $new. Review the diff, commit 'Release $new', merge to main, then:"
echo "  git tag -a v$new -m 'cetra $new' <merged commit> && git push origin v$new"
