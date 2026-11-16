#!/usr/bin/env bash
# Stages the Docker build context of one architecture from a Linux release archive (D-N-3):
#
#   target/dist/image/linux-<arch>/rootfs/usr/local/bin/cetra                    (mode 0755)
#   target/dist/image/linux-<arch>/rootfs/usr/share/doc/cetra/{LICENSE,LICENSE-ELASTIC,THIRD-PARTY-NOTICES.md}
#   target/dist/image/linux-<arch>/datadir/data/                                   (empty; becomes /data)
#
# The image is built from the archive, not from target/<triple>/release, so the release job can
# rebuild the context from downloaded archives (artifact uploads drop modes and empty dirs).
#
# Usage: image-context.sh <archive.tar.gz> <amd64|arm64>
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if [[ $# -ne 2 ]]; then
  echo "usage: image-context.sh <archive.tar.gz> <amd64|arm64>" >&2
  exit 2
fi
archive="$1"
arch="$2"
case "$arch" in
  amd64 | arm64) ;;
  *)
    echo "image-context.sh: unsupported architecture $arch" >&2
    exit 2
    ;;
esac
name="$(basename "$archive" .tar.gz)"
context="$root/target/dist/image/linux-$arch"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
tar -xzf "$archive" -C "$tmp"

rm -rf "$context"
mkdir -p "$context/rootfs/usr/local/bin" "$context/rootfs/usr/share/doc/cetra" "$context/datadir/data"
install -m 0755 "$tmp/$name/cetra" "$context/rootfs/usr/local/bin/cetra"
install -m 0644 "$tmp/$name/LICENSE" "$tmp/$name/LICENSE-ELASTIC" "$tmp/$name/THIRD-PARTY-NOTICES.md" \
  "$context/rootfs/usr/share/doc/cetra/"
echo "wrote target/dist/image/linux-$arch"
