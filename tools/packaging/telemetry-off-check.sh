#!/usr/bin/env bash
# Fails when a script that starts a cetra server for a test does not set CETRA_TELEMETRY=off
# (track TL M4): release binaries in CI containers and kind pods do not see the runner's CI
# variable, so the switchboard's CI rule cannot turn the heartbeat off for them.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
status=0
for script in \
  tools/packaging/binary-smoke.sh \
  tools/packaging/image-smoke.sh \
  tools/packaging/compose-smoke.sh \
  tools/packaging/helm-smoke.sh \
  tools/e2e/run.sh; do
  # An assignment, not a comment: `NAME=off \`, `-e NAME=off`, `secret.values.NAME=off` or a heredoc line.
  if ! grep -Eq '^[[:space:]]*(-e |--set-string secret\.values\.)?CETRA_TELEMETRY=off([[:space:]]|$)' "$root/$script"; then
    echo "telemetry-off-check: $script does not set CETRA_TELEMETRY=off" >&2
    status=1
  fi
done
exit "$status"
