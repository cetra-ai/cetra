# N7 — The install script

Read [README.md](README.md) first, especially § Design › Install script.

**Goal:**
- `deploy/install.sh` installs the right release binary on Linux x86_64/aarch64 and macOS arm64,
  after checking its SHA-256 (D-N-15). Users run it as
  `curl -fsSL https://github.com/sebastiencs/llmobs/releases/latest/download/install.sh | sh`;
  N10 attaches it to every release.
- `just install-smoke` serves `target/dist` on loopback and runs the script against it.

**Edit only:**
- `deploy/install.sh`, `tools/packaging/install-smoke.sh` (new, executable);
- `justfile`.

**Commit:** `Add the install script`

**Tools:** python3, curl, and N3's tools.

## How it works

- **POSIX `sh`.** `curl | sh` runs `dash` on Debian and Ubuntu, so the script has no bashisms.
- **Target.** `uname -s`/`uname -m` map to `x86_64-unknown-linux-musl`,
  `aarch64-unknown-linux-musl` or `aarch64-apple-darwin`. Anything else fails with a pointer to
  the Docker image.
- **Version.** `LLMOBS_VERSION` if set; otherwise the last path segment of the URL that
  `/releases/latest` redirects to (`…/releases/tag/v0.1.0`). Drafts and pre-releases are never
  "latest", so this finds the newest stable release.
- **Download.** It fetches `llmobs-<version>-<target>.tar.gz` and `SHA256SUMS` from
  `LLMOBS_DOWNLOAD_BASE` (default `https://github.com/sebastiencs/llmobs/releases/download/v<version>`),
  checks the archive with `sha256sum`, or `shasum -a 256` on macOS, and extracts it.
- **Install.** It copies the binary to `$dir/llmobs.tmp` and renames it to `$dir/llmobs`, so a
  running `llmobs` is never half-written. `dir` is `LLMOBS_INSTALL_DIR`, default
  `$HOME/.local/bin`.
- **`install-smoke.sh`.** Copies the archives for the current version from `target/dist` into a
  temporary web root and writes their `SHA256SUMS` with N3's `checksums.sh`. It serves the root
  with `python3 -m http.server` on a free loopback port, runs the script with the three variables,
  and checks that the installed `llmobs version` prints the workspace version.

## Files

### `deploy/install.sh` (executable)

```bash
#!/bin/sh
# Installs the llmobs binary from a GitHub release (D-N-15):
#   curl -fsSL https://github.com/sebastiencs/llmobs/releases/latest/download/install.sh | sh
#
# Environment:
#   LLMOBS_VERSION        version to install, e.g. 0.1.0 (default: the latest release)
#   LLMOBS_INSTALL_DIR    where to put `llmobs` (default: $HOME/.local/bin)
#   LLMOBS_DOWNLOAD_BASE  base URL of the release files (default: the GitHub release of
#                         LLMOBS_VERSION); tests point it at a local server
#
# Downloads llmobs-<version>-<target>.tar.gz and SHA256SUMS, checks the archive's checksum,
# and copies the binary. Linux x86_64 and aarch64 (static) and macOS arm64 only.
set -eu

repo="https://github.com/sebastiencs/llmobs"

fail() {
  echo "install.sh: $*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is required"
}

need curl
need tar
need uname

os="$(uname -s)"
machine="$(uname -m)"
case "$os/$machine" in
  Linux/x86_64 | Linux/amd64) target=x86_64-unknown-linux-musl ;;
  Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-musl ;;
  Darwin/arm64) target=aarch64-apple-darwin ;;
  *) fail "no llmobs binary for $os $machine; use the Docker image instead" ;;
esac

version="${LLMOBS_VERSION:-}"
if [ -z "$version" ]; then
  # /releases/latest redirects to /releases/tag/v<version>.
  url="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$repo/releases/latest")" ||
    fail "could not find the latest release"
  version="${url##*/v}"
  case "$version" in
    "" | */*) fail "could not read the latest version from $url" ;;
  esac
fi
version="${version#v}"

if [ -z "${LLMOBS_INSTALL_DIR:-}" ] && [ -z "${HOME:-}" ]; then
  fail "HOME is not set; set LLMOBS_INSTALL_DIR"
fi

base="${LLMOBS_DOWNLOAD_BASE:-$repo/releases/download/v$version}"
name="llmobs-$version-$target"
dir="${LLMOBS_INSTALL_DIR:-$HOME/.local/bin}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading $name.tar.gz"
curl -fsSL -o "$tmp/$name.tar.gz" "$base/$name.tar.gz" || fail "download of $base/$name.tar.gz failed"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || fail "download of $base/SHA256SUMS failed"

expected="$(grep " $name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)"
[ -n "$expected" ] || fail "$name.tar.gz is not listed in SHA256SUMS"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$name.tar.gz" | cut -d' ' -f1)"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d' ' -f1)"
else
  fail "sha256sum or shasum is required"
fi
[ "$actual" = "$expected" ] || fail "checksum mismatch for $name.tar.gz"

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$dir"
cp "$tmp/$name/llmobs" "$dir/llmobs.tmp"
chmod 0755 "$dir/llmobs.tmp"
mv "$dir/llmobs.tmp" "$dir/llmobs"

echo "Installed llmobs $version to $dir/llmobs"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "Add $dir to your PATH to run llmobs." ;;
esac
echo "Licences: LICENSE, LICENSE-ELASTIC and THIRD-PARTY-NOTICES.md in $base/$name.tar.gz"
```

### `tools/packaging/install-smoke.sh` (executable)

```bash
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
archives=(target/dist/llmobs-"$version"-*.tar.gz)
if (( ${#archives[@]} == 0 )); then
  echo "install-smoke: no target/dist/llmobs-$version-*.tar.gz; run just dist <target> first" >&2
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

LLMOBS_VERSION="$version" \
LLMOBS_INSTALL_DIR="$dir/bin" \
LLMOBS_DOWNLOAD_BASE="http://127.0.0.1:$port" \
  sh deploy/install.sh

"$dir/bin/llmobs" version | grep -qx "llmobs $version"
echo "install-smoke: install.sh installed llmobs $version"
```

### `justfile`

Append after the `helm-smoke` recipe:

```just
# Run deploy/install.sh against target/dist served on loopback (D-N-15)
install-smoke:
    tools/packaging/install-smoke.sh
```

## Verify

```bash
export PATH="$HOME/.local/share/nvm/v26.13.0/bin:$PATH"
sh -n deploy/install.sh
just dist x86_64-unknown-linux-musl    # if target/dist has no archive of the current version
just install-smoke
just check
just assert-clean
```

`install-smoke` prints `Downloading llmobs-0.0.0-x86_64-unknown-linux-musl.tar.gz`, then
`Installed llmobs 0.0.0 to /tmp/…/bin/llmobs` and a PATH hint, then
`install-smoke: install.sh installed llmobs 0.0.0`.

The latest-version lookup is not covered by `install-smoke`, which always sets
`LLMOBS_VERSION`. Check the technique by hand on any public repository with releases:
`curl -fsSLI -o /dev/null -w '%{url_effective}\n' https://github.com/clap-rs/clap/releases/latest`
prints `…/releases/tag/v<version>`, and the script keeps what follows the last `/v`.

Negative check, by hand and not committed: corrupt the served archive and the script must stop
with `checksum mismatch`.

1. In `install-smoke.sh`, temporarily add
   `printf x >> "$dir/www/llmobs-$version-x86_64-unknown-linux-musl.tar.gz"` after the
   `checksums.sh` line.
2. Run `just install-smoke` and see it fail with `install.sh: checksum mismatch for …`.
3. Revert the line.

## Done when

- [ ] Both scripts exist with mode `100755`; the recipe is in the `justfile`.
- [ ] `just install-smoke` passes and the negative check fails as described.
- [ ] `just check` and `just assert-clean` pass. One commit: `Add the install script`.
