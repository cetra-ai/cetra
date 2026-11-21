#!/usr/bin/env bash
# Copies the agent skill and its plugin manifests into a checkout of cetra-ai/skills (D-AO-9).
# Usage: tools/skills/mirror.sh <dest> [--push]   (run from the repository root)
# --push commits the copy in <dest> and pushes it to the checkout's origin.
set -euo pipefail

usage() {
    echo "usage: tools/skills/mirror.sh <dest> [--push]" >&2
    exit 2
}

[ "$#" -ge 1 ] || usage
dest="$1"
shift

push=0
if [ "$#" -gt 0 ]; then
    if [ "$#" -ne 1 ] || [ "$1" != "--push" ]; then
        usage
    fi
    push=1
fi

if [ ! -d "$dest" ] || [ ! -e "$dest/.git" ]; then
    echo "mirror.sh: $dest is not a git checkout" >&2
    usage
fi
if [ "$(cd "$dest" && pwd -P)" = "$(pwd -P)" ]; then
    echo "mirror.sh: $dest is the source repository" >&2
    exit 2
fi
if [ ! -d skills/cetra ] || [ ! -d skills/manifests ]; then
    echo "mirror.sh: run from the repository root" >&2
    exit 2
fi

rm -rf "$dest/skills/cetra" "$dest/.claude-plugin" "$dest/.codex-plugin" "$dest/.cursor-plugin" "$dest/README.md" "$dest/LICENSE"
mkdir -p "$dest/skills"
cp -r skills/cetra "$dest/skills/cetra"
cp -r skills/manifests/.claude-plugin skills/manifests/.codex-plugin skills/manifests/.cursor-plugin skills/manifests/README.md skills/manifests/LICENSE "$dest/"
echo "skills: copied to $dest"

if [ "$push" -eq 1 ]; then
    git -C "$dest" add -A
    if git -C "$dest" diff --cached --quiet; then
        echo "skills: nothing to mirror"
        exit 0
    fi
    git -C "$dest" -c user.name="cetra skills mirror" -c user.email="41898282+github-actions[bot]@users.noreply.github.com" commit -m "Mirror cetra-ai/cetra@${SOURCE_SHA:-$(git rev-parse HEAD)}"
    git -C "$dest" push origin HEAD
fi
