#!/bin/sh
# Installs the cetra binary from a GitHub release (D-N-15). cetra.sh/install.sh redirects here (D-W-20):
#   curl -fsSL https://cetra.sh/install.sh | sh
#
# Environment:
#   CETRA_VERSION        version to install, e.g. 0.1.0 (default: the latest release)
#   CETRA_INSTALL_DIR    where to put `cetra` (default: $HOME/.local/bin)
#   CETRA_DOWNLOAD_BASE  base URL of the release files (default: the GitHub release of
#                         CETRA_VERSION); tests point it at a local server
#
# Downloads cetra-<version>-<target>.tar.gz and SHA256SUMS, checks the archive's checksum,
# and copies the binary. Linux x86_64 and aarch64 (static) and macOS arm64 only.
set -eu

repo="https://github.com/cetra-ai/cetra"

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
  *) fail "no cetra binary for $os $machine; use the Docker image instead" ;;
esac

version="${CETRA_VERSION:-}"
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

if [ -z "${CETRA_INSTALL_DIR:-}" ] && [ -z "${HOME:-}" ]; then
  fail "HOME is not set; set CETRA_INSTALL_DIR"
fi

base="${CETRA_DOWNLOAD_BASE:-$repo/releases/download/v$version}"
name="cetra-$version-$target"
dir="${CETRA_INSTALL_DIR:-$HOME/.local/bin}"

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
cp "$tmp/$name/cetra" "$dir/cetra.tmp"
chmod 0755 "$dir/cetra.tmp"
mv "$dir/cetra.tmp" "$dir/cetra"

echo "Installed cetra $version to $dir/cetra"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "Add $dir to your PATH to run cetra." ;;
esac
echo "Licences: LICENSE, LICENSE-ELASTIC and THIRD-PARTY-NOTICES.md in $base/$name.tar.gz"
