#!/usr/bin/env bash
# Prints the PEP 440 form of a release version, for the Python SDK (D-N-37): 0.1.0 stays 0.1.0,
# 0.1.0-alpha.1, -beta.1 and -rc.1 become 0.1.0a1, 0.1.0b1 and 0.1.0rc1. Any other pre-release
# part has no PEP 440 form and is an error.
#
# Usage: pep440.sh <version>
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: pep440.sh <version>" >&2
  exit 2
fi
if [[ "$1" =~ ^([0-9]+\.[0-9]+\.[0-9]+)(-(alpha|beta|rc)\.([0-9]+))?$ ]]; then
  case "${BASH_REMATCH[3]}" in
    "") echo "${BASH_REMATCH[1]}" ;;
    alpha) echo "${BASH_REMATCH[1]}a${BASH_REMATCH[4]}" ;;
    beta) echo "${BASH_REMATCH[1]}b${BASH_REMATCH[4]}" ;;
    rc) echo "${BASH_REMATCH[1]}rc${BASH_REMATCH[4]}" ;;
  esac
else
  echo "pep440.sh: '$1' has no PEP 440 form: use X.Y.Z or X.Y.Z-(alpha|beta|rc).N" >&2
  exit 1
fi
