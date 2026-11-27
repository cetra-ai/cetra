#!/usr/bin/env bash
# Prepares release <version> (D-N-5): sets the workspace version, Cargo.lock, the chart's version
# and appVersion, the compose image, `info.version` of crates/server/openapi.json, the SDK
# versions (D-N-37; PEP 440 form for Python, and its uv.lock) and the cetra-cli version (D-AO-2), and
# turns CHANGELOG.md's Unreleased section into `## [<version>] - <today, UTC>` under a new empty
# Unreleased section. Commit the result, merge it to main, then tag that commit `v<version>`.
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
py_new="$(tools/packaging/pep440.sh "$new")"
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
sed -i.bak "s/^  \"version\": \".*\",$/  \"version\": \"$new\",/" sdks/typescript/package.json
sed -i.bak "s/^export const VERSION = \".*\";$/export const VERSION = \"$new\";/" \
  sdks/typescript/src/index.ts
sed -i.bak "s/^  \"version\": \".*\",$/  \"version\": \"$new\",/" node-cli/package.json
sed -i.bak "s/^export const VERSION = \".*\";$/export const VERSION = \"$new\";/" node-cli/src/version.ts
sed -i.bak "/^\[project\]/,/^\[/{s/^version = \".*\"$/version = \"$py_new\"/;}" sdks/python/pyproject.toml
sed -i.bak "/^  \"info\": {$/,/^  },$/{s/^    \"version\": \".*\"$/    \"version\": \"$new\"/;}" \
  crates/server/openapi.json
today="$(date -u +%Y-%m-%d)"
sed -i.bak "s/^## \[Unreleased\]$/## [Unreleased]\n\n## [$new] - $today/" CHANGELOG.md
rm -f Cargo.toml.bak deploy/helm/cetra/Chart.yaml.bak deploy/compose/compose.yaml.bak \
  deploy/compose/.env.example.bak CHANGELOG.md.bak sdks/typescript/package.json.bak \
  sdks/typescript/src/index.ts.bak node-cli/package.json.bak node-cli/src/version.ts.bak \
  sdks/python/pyproject.toml.bak crates/server/openapi.json.bak

cargo update --workspace --offline
(cd sdks/python && uv lock)
# The capture harness, its project and each scenario environment lock cetra-sdk's version.
for dir in tools/capture/harness tools/capture/py tools/capture/envs/*/; do
  [ -f "$dir/uv.lock" ] || continue
  (cd "$dir" && uv lock)
done
tools/packaging/versions-check.sh
# A stable release writes its golden data directory (spec 03 § 12, D-R-30) and its golden server
# backup (spec 04 § Format versions, D-R-38) with this build; a pre-release gets neither.
if [[ "$new" != *-* ]]; then
  just golden-dir "$new"
fi
echo "release-prep: $old -> $new. Review the diff, commit 'Release $new', merge to main, then:"
echo "  git tag -a v$new -m 'cetra $new' <merged commit> && git push origin v$new"
