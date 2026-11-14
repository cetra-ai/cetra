#!/usr/bin/env bash
# Prints the image tags of a release, comma-separated, for docker/build-push-action (D-N-6):
# <image>:<version> always; for a version without a pre-release part also <image>:<major>.<minor>,
# <image>:<major> when major > 0, and <image>:latest.
#
# Usage: image-tags.sh <image> <version>, e.g. image-tags.sh ghcr.io/owner/llmobs 0.1.0
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
