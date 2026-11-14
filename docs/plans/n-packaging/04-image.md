# N4 — The Docker image

Read [README.md](README.md) first, especially § Design › Image.

**Goal:**
- `just image [tag]` builds the llmobs image for this machine's architecture from
  `target/dist/image/linux-<arch>/` (N3) (D-N-3).
- `just image-smoke [tag]` runs it with a fresh volume and checks the smoke test, the user
  (65532) and the Docker health state.

The release workflow (N10) builds the same Dockerfile for `linux/amd64` and `linux/arm64` at once.
That works without emulation because the Dockerfile has no `RUN`.

**Edit only:**
- `deploy/docker/Dockerfile`, `.dockerignore`, `tools/packaging/image-smoke.sh` (new);
- `justfile`.

**Commit:** `Add the Docker image`

**Tools:** Docker Engine 25+ with BuildKit (the default builder), and N3's tools.

## How it works

- The base is `gcr.io/distroless/static-debian13:nonroot`, pinned by the digest of its multi-arch
  index. `nonroot` already sets user 65532; the Dockerfile repeats `USER 65532:65532` so
  `docker image inspect` shows it.
- `ARG TARGETARCH` is set by BuildKit (`amd64` or `arm64`) and picks the staged directory.
- The first `COPY` puts the binary and `/usr/share/doc/llmobs/*` in place, owned by root.
- The second `COPY --chown=65532:65532 …/datadir/ /` creates `/data` owned by the runtime user. A
  new named volume copies that owner, so the volume is writable without an init container.
- `.dockerignore` keeps the build context to `target/dist/image/`. Without it, Docker would send
  the whole repository, `target/` included. Each directory level is re-included and its other
  children excluded again, so it works however the builder walks the tree.
- `HEALTHCHECK` runs `llmobs health` (N1) every 30 s, with a 120 s start period: opening storage
  replays the WAL before the ports open.

## Files

### `deploy/docker/Dockerfile` (new)

```dockerfile
# The llmobs image (D-N-3). Built from the release binaries that tools/packaging/dist.sh stages
# in target/dist/image/linux-<arch>/; nothing is compiled here, so a multi-arch build needs no
# emulation. Build from the repository root:
#   docker build -f deploy/docker/Dockerfile -t llmobs:dev .
FROM gcr.io/distroless/static-debian13:nonroot@sha256:aafe993665bf6200db38a02c11a9351e782a84b6a45e7f33b6695638c88e3931

ARG TARGETARCH

# The binary and the licence notices, owned by root.
COPY target/dist/image/linux-${TARGETARCH}/rootfs/ /
# /data, owned by the runtime user, so a new named volume is writable (Docker copies the
# directory's owner into an empty volume).
COPY --chown=65532:65532 target/dist/image/linux-${TARGETARCH}/datadir/ /

ENV LLMOBS_DATA_DIR=/data
USER 65532:65532
VOLUME ["/data"]
# 8080: UI, API, OTLP/HTTP, MCP. 4317: OTLP/gRPC. 4318: OTLP/HTTP (S11).
EXPOSE 8080 4317 4318
HEALTHCHECK --interval=30s --timeout=10s --start-period=120s --retries=3 \
  CMD ["/usr/local/bin/llmobs", "health"]
ENTRYPOINT ["/usr/local/bin/llmobs"]
CMD ["serve"]
```

### `.dockerignore` (new, repository root)

```text
# The image is built from staged release binaries only (deploy/docker/Dockerfile, D-N-3).
# Each level is re-included explicitly, which works whichever way the builder walks the tree.
*
!target
target/*
!target/dist
target/dist/*
!target/dist/image
```

### `tools/packaging/image-smoke.sh` (new, executable)

```bash
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
```

### `justfile`

Append after the `checksums` recipe:

```just
# Build the image for this machine's architecture from target/dist/image (run `just dist` first) (D-N-3)
image tag="llmobs:dev":
    docker build -f deploy/docker/Dockerfile -t {{tag}} .

# Run an image with a fresh volume and smoke-test it, healthcheck included (D-N-13)
image-smoke tag="llmobs:dev":
    tools/packaging/image-smoke.sh {{tag}}
```

## Verify

```bash
export PATH="$HOME/.local/share/nvm/v26.13.0/bin:$PATH"
just dist x86_64-unknown-linux-musl    # skip if target/dist/image/linux-amd64 is from this commit's N3 build
just image llmobs:dev
docker image inspect --format '{{.Config.User}} {{.Config.Entrypoint}} {{.Config.Cmd}} {{.Config.ExposedPorts}}' llmobs:dev
docker run --rm llmobs:dev version
just image-smoke llmobs:dev
just check
just assert-clean
```

Expected:
- `inspect` prints `65532:65532 [/usr/local/bin/llmobs] [serve] map[4317/tcp:{} 4318/tcp:{} 8080/tcp:{}]`.
- `docker run --rm llmobs:dev version` prints `llmobs 0.0.0` and `ui: embedded`.
- `image-smoke` prints the four `smoke:` lines and then `image-smoke: llmobs:dev passed`. The
  health state turns `healthy` about 30 s after the start, at the first health check.
- `docker images llmobs:dev` shows the binary's size plus about 2 MB of distroless.

If `image-smoke` times out on `/readyz` and `docker logs` shows a permission error on `/data`,
the empty `data/` directory did not reach the image: check
`ls -ld target/dist/image/linux-amd64/datadir/data` and
`docker run --rm --entrypoint /usr/local/bin/llmobs llmobs:dev version` (which only proves the
binary runs), then stop and report. Do not add a `RUN` step or a base image with a shell.

If `docker build` reports that `target/dist/image/linux-amd64/...` is not found, run
`just dist x86_64-unknown-linux-musl` first. On an arm64 machine the directory is `linux-arm64`
and the target is `aarch64-unknown-linux-musl`.

## Done when

- [ ] The three files exist; `image-smoke.sh` is mode `100755`; the two recipes are in the
      `justfile`.
- [ ] The commands above pass.
- [ ] `just check` and `just assert-clean` pass. One commit: `Add the Docker image`.
