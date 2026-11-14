#!/usr/bin/env bash
# Runs an llmobs image with a fresh volume and runs smoke.sh against it, then waits for
# Docker's healthcheck (`llmobs health`) to report healthy (D-N-3, D-N-13).
#
# Usage: image-smoke.sh <image>, e.g. image-smoke.sh llmobs:dev
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if [[ $# -ne 1 ]]; then
  echo "usage: image-smoke.sh <image>" >&2
  exit 2
fi
image="$1"
name="llmobs-smoke-$$"
volume="llmobs-smoke-$$"
key="sk-llmobs-SMOKE0123456789ABCDEFGHIJKLMNOPQ"

cleanup() {
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker volume rm "$volume" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# The image runs as uid 65532 and must not need root.
user="$(docker image inspect --format '{{.Config.User}}' "$image")"
if [[ "$user" != "65532:65532" ]]; then
  echo "image-smoke: $image runs as '$user', expected 65532:65532" >&2
  exit 1
fi

docker volume create "$volume" >/dev/null
docker run -d --name "$name" \
  -v "$volume:/data" \
  -p 127.0.0.1::8080 -p 127.0.0.1::4318 \
  -e LLMOBS_INIT_ADMIN_EMAIL=smoke@example.com \
  -e LLMOBS_INIT_ADMIN_PASSWORD=llmobs-smoke-1 \
  -e LLMOBS_INIT_ORG=Smoke \
  -e LLMOBS_INIT_PROJECT=Smoke \
  -e LLMOBS_INIT_SECRET_KEY="$key" \
  "$image" >/dev/null

http_port="$(docker port "$name" 8080/tcp | head -n1 | sed 's/.*://')"
otlp_port="$(docker port "$name" 4318/tcp | head -n1 | sed 's/.*://')"
if ! "$root/tools/packaging/smoke.sh" "http://127.0.0.1:${http_port}" "http://127.0.0.1:${otlp_port}" "$key"; then
  docker logs "$name" >&2
  exit 1
fi

# The healthcheck's first run is up to --interval (30 s) after the start.
for _ in $(seq 1 90); do
  health="$(docker inspect --format '{{.State.Health.Status}}' "$name")"
  if [[ "$health" == "healthy" ]]; then
    echo "image-smoke: $image passed"
    exit 0
  fi
  sleep 1
done
docker logs "$name" >&2
echo "image-smoke: the container is '$health', not healthy, after 90 s" >&2
exit 1
