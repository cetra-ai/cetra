# N8 — Versions, changelog and release scripts

Read [README.md](README.md) first, especially § Design › Versions, changelog, publication.

**Goal:** one version for everything, kept in sync by scripts (D-N-5, D-N-6):
- `CHANGELOG.md` (Keep a Changelog) with an `## [Unreleased]` section.
- `just versions-check`: the chart and the compose files carry the workspace version.
- `just release-prep <version>`: sets the version everywhere, refreshes `Cargo.lock` and dates the
  changelog section.
- `just release-notes <version>`: prints that section, which becomes the GitHub release notes.
- `just release-verify <tag>`: the release workflow's gate. It checks four things:
  - the tag is `v<version>`;
  - the files agree;
  - the changelog section exists;
  - the tagged commit is on `origin/main`.
- `tools/packaging/image-tags.sh`: the image tags of a version, used by N10.

**Edit only:**
- `CHANGELOG.md`, `tools/packaging/versions-check.sh`, `tools/packaging/release-prep.sh`,
  `tools/packaging/release-notes.sh`, `tools/packaging/release-verify.sh`,
  `tools/packaging/image-tags.sh` (new; scripts executable);
- `justfile`;
- `AGENTS.md`.

**Commit:** `Add the changelog and release scripts`

**Tools:** the usual ones (GNU sed: `release-prep.sh` uses `\n` in a `sed` replacement and runs on
Linux only).

## How it works

- **The version.** It lives in one place, `[workspace.package] version` in the root `Cargo.toml`;
  `tools/packaging/version.sh` (N3) prints it. Every crate inherits it.
  `release-prep.sh <new>` does the rest:
  1. Checks that `<new>` is SemVer without build metadata.
  2. Checks that it differs from the current version and has no changelog section yet.
  3. Checks that `## [Unreleased]` is not empty.
  4. Rewrites `Cargo.toml`'s version line inside `[workspace.package]`, `Chart.yaml`'s `version:`
     and `appVersion:`, the `image:` default of `compose.yaml` and the `#CETRA_IMAGE=` line of
     `.env.example`.
  5. Inserts `## [<new>] - <UTC date>` under a fresh empty `## [Unreleased]`.
  6. Runs `cargo update --workspace --offline`, so the workspace crates' versions in
     `Cargo.lock` follow.
  7. Runs `versions-check.sh`.
- **`versions-check.sh`.** Looks for four exact lines. N9 runs it in CI on every PR, so a hand
  edit of one file fails.
- **`release-notes.sh <version>`.** Prints the lines between `## [<version>] - ` and the next
  `## [`. It fails if the section is missing or blank.
- **`release-verify.sh <tag>`.** Needs `origin/main` fetched; the workflow fetches it.
- **`image-tags.sh <image> <version>`.** Always prints `<image>:<version>`. For a version without
  a pre-release part it adds `<image>:<major>.<minor>`, `<image>:<major>` when major > 0, and
  `<image>:latest`. Output is comma-separated, the format of `docker/build-push-action`'s `tags`.

**Changelog rule** (written into `AGENTS.md` below): a PR with a user-visible change adds a line to
`## [Unreleased]` under `### Added`, `### Changed`, `### Fixed` or `### Removed`. A change that
needs action on upgrade adds it under `### Upgrade notes`, which comes first in the section.

## Files

### `CHANGELOG.md` (new, repository root)

```markdown
# Changelog

All notable changes to cetra are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (D-N-5). A release that needs action on upgrade starts
with an `### Upgrade notes` section (docs/self-hosting/upgrade.md).

## [Unreleased]

### Added

- Release archives for Linux x86_64 and aarch64 (static) and macOS arm64, an install script, a
  Docker image on GHCR, docker-compose files (local volume, or S3 with Garage) and a Helm chart,
  with checksums, build provenance attestations, SBOMs and third-party licence notices (track N).
```

### `tools/packaging/versions-check.sh`

```bash
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
```

### `tools/packaging/release-prep.sh`

```bash
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
echo "release-prep: $old -> $new. Review the diff, commit 'Release $new', merge to main, then:"
echo "  git tag -a v$new -m 'cetra $new' <merged commit> && git push origin v$new"
```

### `tools/packaging/release-notes.sh`

```bash
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
```

### `tools/packaging/release-verify.sh`

```bash
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
if ! git merge-base --is-ancestor HEAD origin/main; then
  echo "release-verify: $tag is not on origin/main" >&2
  exit 1
fi
echo "release-verify: $tag is ready"
```

### `tools/packaging/image-tags.sh`

```bash
#!/usr/bin/env bash
# Prints the image tags of a release, comma-separated, for docker/build-push-action (D-N-6):
# <image>:<version> always; for a version without a pre-release part also <image>:<major>.<minor>,
# <image>:<major> when major > 0, and <image>:latest.
#
# Usage: image-tags.sh <image> <version>, e.g. image-tags.sh ghcr.io/owner/cetra 0.1.0
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: image-tags.sh <image> <version>" >&2
  exit 2
fi
image="$1"
version="$2"
tags="$image:$version"
if [[ "$version" != *-* ]]; then
  major="${version%%.*}"
  rest="${version#*.}"
  minor="${rest%%.*}"
  tags="$tags,$image:$major.$minor"
  if [[ "$major" != "0" ]]; then
    tags="$tags,$image:$major"
  fi
  tags="$tags,$image:latest"
fi
echo "$tags"
```

### `justfile`

Append after the `install-smoke` recipe:

```just
# Check that the chart and compose files carry the workspace version (D-N-5)
versions-check:
    tools/packaging/versions-check.sh

# Set the release version everywhere and date the changelog section, e.g. `just release-prep 0.1.0` (D-N-5)
release-prep version:
    tools/packaging/release-prep.sh {{version}}

# Release workflow gate: tag = version, files agree, changelog section, tag on origin/main (D-N-5)
release-verify tag:
    tools/packaging/release-verify.sh {{tag}}

# Print a version's changelog section (the GitHub release notes)
release-notes version:
    tools/packaging/release-notes.sh {{version}}
```

`image-tags.sh` has no recipe: only the release workflow calls it, in a step that already runs a
script.

### `AGENTS.md`

1. In § Commands, add these rows to the table, after the `just e2e` row:

   ```markdown
   | `just dist <target>` | release archive of one target into `target/dist` (and the image context on Linux) |
   | `just image [tag]` · `just image-smoke [tag]` | build and smoke-test the Docker image from `target/dist` |
   | `just compose-smoke [tag] [s3]` · `just helm-check` · `just helm-smoke [tag]` · `just install-smoke` | packaging smoke tests (D-N-13) |
   | `just versions-check` | the chart and compose files carry the workspace version |
   | `just release-prep <version>` | set a release version everywhere and date the changelog (D-N-5) |
   ```

2. In § Commits and reports, add this bullet after "Don't commit generated artifacts …":

   ```markdown
   - A user-visible change adds a line to `CHANGELOG.md` § `[Unreleased]` (`Added`, `Changed`, `Fixed`, `Removed`); one that needs action on upgrade goes under `### Upgrade notes` (D-N-5).
   ```

## Verify

Commit first (`Add the changelog and release scripts`): the throwaway worktree below checks out
`HEAD`, so it must contain this sub-phase's files. If a check then fails, fix it and
`git commit --amend`. Test `release-prep` on that throwaway copy, never on your branch:

```bash
just versions-check
tmp="$(mktemp -d)" && git worktree add --detach "$tmp" HEAD
(cd "$tmp" && just release-prep 0.1.0-rc.1 && git diff --stat && just release-notes 0.1.0-rc.1 && tools/packaging/release-verify.sh v0.1.0-rc.1; echo "verify exit=$?")
git worktree remove --force "$tmp"
tools/packaging/image-tags.sh ghcr.io/o/cetra 0.1.0
tools/packaging/image-tags.sh ghcr.io/o/cetra 0.1.0-rc.1
tools/packaging/image-tags.sh ghcr.io/o/cetra 1.2.3
just check
just assert-clean
```

Expected:
- `versions-check` prints `versions-check: every file says 0.0.0`.
- In the worktree:
  - `release-prep` ends with the tag hint;
  - `git diff --stat` lists `CHANGELOG.md`, `Cargo.lock`, `Cargo.toml`,
    `deploy/compose/.env.example`, `deploy/compose/compose.yaml` and
    `deploy/helm/cetra/Chart.yaml`;
  - `release-notes` prints the section, starting with its `### Added` line;
  - `release-verify` fails with `… is not on origin/main`, because the commit is not merged.
    That shows the check works: `verify exit=1`.
- `image-tags.sh` prints:
  - `ghcr.io/o/cetra:0.1.0,ghcr.io/o/cetra:0.1,ghcr.io/o/cetra:latest`;
  - `ghcr.io/o/cetra:0.1.0-rc.1`;
  - `ghcr.io/o/cetra:1.2.3,ghcr.io/o/cetra:1.2,ghcr.io/o/cetra:1,ghcr.io/o/cetra:latest`.

## Done when

- [ ] `CHANGELOG.md` and the five scripts exist (scripts mode `100755`); the four recipes and
      the `AGENTS.md` lines are in.
- [ ] The commands above behave as described.
- [ ] `just check` and `just assert-clean` pass. One commit:
      `Add the changelog and release scripts`.
