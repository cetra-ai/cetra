#!/usr/bin/env bash
# Builds the release archive of one target into target/dist (D-N-1, D-N-16):
#
#   target/dist/cetra-<version>-<target>.tar.gz
#     cetra-<version>-<target>/cetra                   the binary, debug info stripped
#     cetra-<version>-<target>/LICENSE                  the root LICENSE (MIT, names the ee dirs)
#     cetra-<version>-<target>/LICENSE-ELASTIC          crates/server/src/ee/LICENSE (ELv2)
#     cetra-<version>-<target>/THIRD-PARTY-NOTICES.md
#
# and, for a Linux target, the Docker build context of that architecture,
# target/dist/image/linux-<amd64|arm64>/ (image-context.sh, D-N-3).
#
# Usage: dist.sh <target>, one of x86_64-unknown-linux-musl, aarch64-unknown-linux-musl,
# aarch64-apple-darwin. Run it on a host of the same OS and architecture: the binary is
# executed to check that the UI is embedded. Linux targets need musl-gcc (Ubuntu: musl-tools).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"

if [[ $# -ne 1 ]]; then
  echo "usage: dist.sh <target>" >&2
  exit 2
fi
target="$1"
case "$target" in
  x86_64-unknown-linux-musl)
    arch=amd64
    export CC_x86_64_unknown_linux_musl=musl-gcc
    ;;
  aarch64-unknown-linux-musl)
    arch=arm64
    export CC_aarch64_unknown_linux_musl=musl-gcc
    ;;
  aarch64-apple-darwin)
    arch=""
    ;;
  *)
    echo "dist.sh: unsupported target $target" >&2
    exit 2
    ;;
esac

for file in LICENSE crates/server/src/ee/LICENSE; do
  if [[ ! -f "$file" ]]; then
    echo "dist.sh: $file is missing (track P's M1, P1, adds it)" >&2
    exit 1
  fi
done

version="$(tools/packaging/version.sh)"
name="cetra-${version}-${target}"

rustup target add "$target"
pnpm --dir web run build
cargo auditable build --release --locked --target "$target" -p cetra-cli
bin="target/${target}/release/cetra"

# The binary must run here and embed the UI (D-G-37).
"$bin" version | grep -qx 'ui: embedded'
"$bin" version | grep -qx "cetra ${version}"
if [[ "$target" == *-linux-musl ]]; then
  # Static: no interpreter, no shared libraries (D-N-1).
  if ! file "$bin" | grep -q 'static'; then
    file "$bin" >&2
    echo "dist.sh: $bin is not statically linked" >&2
    exit 1
  fi
fi

tools/packaging/notices.sh

stage="target/dist/${name}"
rm -rf "$stage" "target/dist/${name}.tar.gz"
mkdir -p "$stage"
install -m 0755 "$bin" "$stage/cetra"
# Drop the DWARF debug info (about 75 % of the file) but keep the symbol table, so backtraces
# still name functions, and the `.dep-v0` section cargo auditable adds (D-N-16).
if [[ "$target" == *-apple-darwin ]]; then
  strip -S "$stage/cetra"
else
  strip --strip-debug "$stage/cetra"
fi
"$stage/cetra" version | grep -qx 'ui: embedded'
install -m 0644 LICENSE "$stage/LICENSE"
install -m 0644 crates/server/src/ee/LICENSE "$stage/LICENSE-ELASTIC"
install -m 0644 target/dist/THIRD-PARTY-NOTICES.md "$stage/THIRD-PARTY-NOTICES.md"
tar -czf "target/dist/${name}.tar.gz" -C target/dist "$name"
echo "wrote target/dist/${name}.tar.gz"

if [[ -n "$arch" ]]; then
  tools/packaging/image-context.sh "target/dist/${name}.tar.gz" "$arch"
fi
