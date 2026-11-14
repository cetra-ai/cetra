# N5 — docker-compose

Read [README.md](README.md) first, especially § Design › docker-compose.

**Goal:**
- `deploy/compose/` runs llmobs on one machine with a named volume (`compose.yaml`).
- An overlay, `compose.s3.yaml`, moves the Parquet files and media to a bucket on a local
  single-node Garage (D-N-9).
- `just compose-smoke [tag] [s3]` runs either variant from a temporary copy and smoke-tests it. In
  S3 mode it also checks that a flush wrote objects to the bucket.

**Edit only:**
- `deploy/compose/compose.yaml`, `deploy/compose/compose.s3.yaml`, `deploy/compose/garage.toml`,
  `deploy/compose/garage-init.sh`, `deploy/compose/llmobs.env.example`,
  `deploy/compose/.env.example`, `tools/packaging/compose-smoke.sh` (new);
- `.gitignore`;
- `justfile`.

**Commit:** `Add docker-compose files`

**Tools:** Docker Compose v2.24 or later (`docker compose version`), python3, and N4's tools.

## How it works

- **Two settings files.** Compose reads `.env` to fill in `${…}` in the compose files: image,
  bind address, ports, and the Garage secrets. Server settings go in `llmobs.env`, passed whole
  with `env_file` (`required: false`, so the file is optional). They are separate because the
  server's config loader:
  - rejects unknown `LLMOBS_*` variables, so `LLMOBS_IMAGE` in the server's environment would
    stop it;
  - treats `LLMOBS_X=` (empty) as a set value.

  Compose therefore passes no `${VAR:-}` defaults to the server.
- **Image default.** It is the workspace version, `0.0.0` now. N8's `release-prep` rewrites the
  `image:` line and the `#LLMOBS_IMAGE=` comment of `.env.example`, and `versions-check` checks
  both lines, so keep them exactly as given.
- **Garage:**
  - `dxflrs/garage:v2.5.0` is a scratch image whose binary is `/garage`. It reads
    `/etc/garage.toml`, and takes `GARAGE_RPC_SECRET` and `GARAGE_ADMIN_TOKEN` from the
    environment.
  - Its healthcheck is `/garage status`.
  - `garage-init` (`curlimages/curl:8.23.0`, which has `/bin/sh`) runs `garage-init.sh` against
    the admin API v2 on port 3903. The calls are `GetClusterStatus`, `GetNodeInfo`,
    `UpdateClusterLayout`, `ApplyClusterLayout`, `GetKeyInfo`, `ImportKey`, `GetBucketInfo`,
    `CreateBucket` and `AllowBucketKey`. Each step checks what exists, so a second `up` succeeds. Garage pretty-prints its JSON (`"layoutVersion": 0`), so each
    answer goes through `tr -d ' \n'` before the `grep -o` that reads a field.
  - The access key is imported from `.env`. Garage's documentation reserves `ImportKey` for
    migrations because a hand-made key ID can collide. The commands in `.env.example` generate
    random IDs in Garage's own format (`GK` + 24 hex, secret 64 hex), which is equivalent.
- **llmobs in S3 mode.** It starts after `garage-init` completed successfully, with:
  - `LLMOBS_OBJECT_STORE=s3://llmobs`;
  - endpoint `http://garage:3900`, path style, `allow_http`;
  - `AWS_REGION=garage`, which is `s3_region` in `garage.toml`;
  - the key.
- **`compose-smoke.sh`:**
  1. Copies `deploy/compose/.` into a temporary directory, so `.env` and `llmobs.env` never
     appear in the checkout.
  2. Picks free ports and writes both files with random Garage secrets.
  3. Sets `LLMOBS_STORAGE__FLUSH__MAX_AGE=5s` so S3 mode flushes quickly.
  4. Runs `docker compose -p llmobs-smoke-<pid> up -d`, then `smoke.sh`.
  5. In S3 mode, polls Garage's `GetBucketInfo` from a throwaway `garage-init` container until
     `"objects"` is not 0.
  6. Always runs `down -v`.

  It does not use `up --wait`, because `garage-init` exits by design.

## Files

### `deploy/compose/compose.yaml`

```yaml
# llmobs on one machine, data on a local volume (D-N-9).
#   cp llmobs.env.example llmobs.env    # server settings: any LLMOBS_* key of spec 12
#   docker compose up -d
# With S3-compatible object storage (a local Garage) for Parquet files and media:
#   cp .env.example .env                # and set the Garage keys and secrets in it
#   docker compose -f compose.yaml -f compose.s3.yaml up -d
# Docs: docs/self-hosting/docker-compose.md
name: llmobs

services:
  llmobs:
    image: ${LLMOBS_IMAGE:-ghcr.io/sebastiencs/llmobs:0.0.0}
    restart: unless-stopped
    # Shutdown drains requests (20 s) and flushes memtables (30 s) (spec 12 § Process lifecycle).
    stop_grace_period: 60s
    ports:
      - "${LLMOBS_BIND:-127.0.0.1}:${LLMOBS_HTTP_PORT:-8080}:8080"
      - "${LLMOBS_BIND:-127.0.0.1}:${LLMOBS_OTLP_GRPC_PORT:-4317}:4317"
      - "${LLMOBS_BIND:-127.0.0.1}:${LLMOBS_OTLP_HTTP_PORT:-4318}:4318"
    # Server settings. Only variables present in the file reach the server: an empty
    # LLMOBS_* variable is a set (and usually invalid) value, not an unset one.
    env_file:
      - path: llmobs.env
        required: false
    volumes:
      - llmobs-data:/data
    healthcheck:
      test: ["CMD", "/usr/local/bin/llmobs", "health"]
      interval: 30s
      timeout: 10s
      start_period: 120s
      start_interval: 2s
      retries: 3

volumes:
  llmobs-data:
```

### `deploy/compose/compose.s3.yaml`

```yaml
# Overlay of compose.yaml: Parquet files and media go to a bucket on a local single-node Garage
# (S3-compatible) instead of the llmobs volume, which keeps the WAL, catalog.db, meta.db and
# secret.key (D-N-9). Set the four GARAGE_* values in .env first (.env.example).
#   docker compose -f compose.yaml -f compose.s3.yaml up -d
# To use another S3-compatible service instead, drop this file and set LLMOBS_OBJECT_STORE,
# LLMOBS_STORAGE__S3__* and AWS_* in llmobs.env (docs/self-hosting/s3.md).
services:
  garage:
    image: dxflrs/garage:v2.5.0
    restart: unless-stopped
    environment:
      GARAGE_RPC_SECRET: ${GARAGE_RPC_SECRET:?set GARAGE_RPC_SECRET in .env}
      GARAGE_ADMIN_TOKEN: ${GARAGE_ADMIN_TOKEN:?set GARAGE_ADMIN_TOKEN in .env}
    volumes:
      - ./garage.toml:/etc/garage.toml:ro
      - garage-meta:/var/lib/garage/meta
      - garage-data:/var/lib/garage/data
    healthcheck:
      test: ["CMD", "/garage", "status"]
      interval: 10s
      timeout: 5s
      start_period: 30s
      start_interval: 1s
      retries: 3

  garage-init:
    image: curlimages/curl:8.23.0
    restart: "no"
    depends_on:
      garage:
        condition: service_healthy
    environment:
      GARAGE_ADMIN_TOKEN: ${GARAGE_ADMIN_TOKEN:?set GARAGE_ADMIN_TOKEN in .env}
      GARAGE_ACCESS_KEY_ID: ${GARAGE_ACCESS_KEY_ID:?set GARAGE_ACCESS_KEY_ID in .env}
      GARAGE_SECRET_ACCESS_KEY: ${GARAGE_SECRET_ACCESS_KEY:?set GARAGE_SECRET_ACCESS_KEY in .env}
    volumes:
      - ./garage-init.sh:/garage-init.sh:ro
    entrypoint: ["/bin/sh", "/garage-init.sh"]

  llmobs:
    depends_on:
      garage-init:
        condition: service_completed_successfully
    environment:
      LLMOBS_OBJECT_STORE: s3://llmobs
      LLMOBS_STORAGE__S3__ENDPOINT: http://garage:3900
      LLMOBS_STORAGE__S3__FORCE_PATH_STYLE: "true"
      LLMOBS_STORAGE__S3__ALLOW_HTTP: "true"
      AWS_REGION: garage
      AWS_ACCESS_KEY_ID: ${GARAGE_ACCESS_KEY_ID:?set GARAGE_ACCESS_KEY_ID in .env}
      AWS_SECRET_ACCESS_KEY: ${GARAGE_SECRET_ACCESS_KEY:?set GARAGE_SECRET_ACCESS_KEY in .env}

volumes:
  garage-meta:
  garage-data:
```

### `deploy/compose/garage.toml`

```toml
# Single-node Garage for compose.s3.yaml (D-N-9). The RPC secret and the admin token come from
# the GARAGE_RPC_SECRET and GARAGE_ADMIN_TOKEN environment variables (.env).
metadata_dir = "/var/lib/garage/meta"
data_dir = "/var/lib/garage/data"
db_engine = "sqlite"
replication_factor = 1

rpc_bind_addr = "[::]:3901"
rpc_public_addr = "127.0.0.1:3901"

[s3_api]
s3_region = "garage"
api_bind_addr = "[::]:3900"

[admin]
api_bind_addr = "[::]:3903"
```

### `deploy/compose/garage-init.sh` (executable)

```bash
#!/bin/sh
# One-shot setup of the single-node Garage of compose.s3.yaml through its admin API (v2):
# a layout with this node, the access key from .env, and the bucket `llmobs` with read and
# write for that key. Safe to run again: each step checks what already exists (D-N-9).
set -eu

api="http://garage:3903"
auth="Authorization: Bearer ${GARAGE_ADMIN_TOKEN}"

call() {
  # call <method> <path> [json body]: prints the body; fails on a non-2xx answer.
  if [ $# -eq 3 ]; then
    curl -sS -f -X "$1" -H "$auth" -H 'Content-Type: application/json' --data "$3" "$api$2"
  else
    curl -sS -f -X "$1" -H "$auth" "$api$2"
  fi
}

i=0
until call GET /v2/GetClusterStatus >/dev/null 2>&1; do
  i=$((i + 1))
  if [ "$i" -ge 60 ]; then
    echo "garage-init: the Garage admin API did not answer within 60 s" >&2
    exit 1
  fi
  sleep 1
done

layout_version="$(call GET /v2/GetClusterStatus | tr -d ' \n' | grep -o '"layoutVersion":[0-9]*' | cut -d: -f2)"
if [ -z "$layout_version" ]; then
  echo "garage-init: no layoutVersion in the GetClusterStatus answer" >&2
  exit 1
fi
if [ "$layout_version" = "0" ]; then
  node="$(call GET '/v2/GetNodeInfo?node=self' | tr -d ' \n' | grep -o '"nodeId":"[0-9a-f]*"' | head -n1 | cut -d'"' -f4)"
  call POST /v2/UpdateClusterLayout \
    "{\"roles\":[{\"id\":\"${node}\",\"zone\":\"dc1\",\"capacity\":100000000000,\"tags\":[]}]}" >/dev/null
  call POST /v2/ApplyClusterLayout '{"version":1}' >/dev/null
  echo "garage-init: layout applied for node ${node}"
fi

if ! call GET "/v2/GetKeyInfo?id=${GARAGE_ACCESS_KEY_ID}" >/dev/null 2>&1; then
  call POST /v2/ImportKey \
    "{\"accessKeyId\":\"${GARAGE_ACCESS_KEY_ID}\",\"secretAccessKey\":\"${GARAGE_SECRET_ACCESS_KEY}\",\"name\":\"llmobs\"}" >/dev/null
  echo "garage-init: access key imported"
fi

bucket="$(call GET '/v2/GetBucketInfo?globalAlias=llmobs' 2>/dev/null | tr -d ' \n' | grep -o '"id":"[0-9a-f]\{64\}"' | head -n1 | cut -d'"' -f4 || true)"
if [ -z "$bucket" ]; then
  bucket="$(call POST /v2/CreateBucket '{"globalAlias":"llmobs"}' | tr -d ' \n' | grep -o '"id":"[0-9a-f]\{64\}"' | head -n1 | cut -d'"' -f4)"
  echo "garage-init: bucket llmobs created"
fi

call POST /v2/AllowBucketKey \
  "{\"bucketId\":\"${bucket}\",\"accessKeyId\":\"${GARAGE_ACCESS_KEY_ID}\",\"permissions\":{\"read\":true,\"write\":true,\"owner\":false}}" >/dev/null
echo "garage-init: done"
```

### `deploy/compose/llmobs.env.example`

```bash
# llmobs server settings for compose.yaml (copy to llmobs.env). Every key of spec 12 can be set
# as LLMOBS_ + the upper-snake path, e.g. LLMOBS_STORAGE__FLUSH__MAX_AGE=60s. Leave a line
# commented out rather than empty: an empty value is a set value. The data directory is /data
# (set by the image).

# The URL users open; links in emails and OAuth redirects use it.
LLMOBS_PUBLIC_URL=http://localhost:8080

# First start only: create the first admin, org, project and an ingest key without the setup
# page. Set the first four together. The key is sk-llmobs- followed by 32 letters or digits.
#LLMOBS_INIT_ADMIN_EMAIL=admin@example.com
#LLMOBS_INIT_ADMIN_PASSWORD=change-me
#LLMOBS_INIT_ORG=My org
#LLMOBS_INIT_PROJECT=My project
#LLMOBS_INIT_SECRET_KEY=sk-llmobs-0123456789abcdefghijklmnopqrstuv

# Email (invites, password reset). Unset: email is off.
#LLMOBS_SMTP_URL=smtps://user:password@smtp.example.com:465
#LLMOBS_EMAIL_FROM=llmobs <llmobs@example.com>

# Encryption key for stored secrets (32 bytes, standard base64). Unset: generated on the first
# start into /data/secret.key; back it up with the data (docs/self-hosting/backup.md).
#LLMOBS_ENCRYPTION_KEY=

# Enterprise licence key (D172). Unset: core features only.
#LLMOBS_LICENSE_KEY=
```

Track P adds the `license_key` setting. Before writing this file, run
`grep -q license_key crates/server/src/config.rs`. If it fails, leave out the last three lines
(the licence comment, `#LLMOBS_LICENSE_KEY=` and the empty line before them), and say so in your
report.

### `deploy/compose/.env.example`

```bash
# Compose settings (copy to .env). These choose the image and the published ports, and hold the
# Garage secrets of compose.s3.yaml. They are not passed to the llmobs server: its settings go
# in llmobs.env.

# Image. Pin a version; there is no `latest` in the docs on purpose.
#LLMOBS_IMAGE=ghcr.io/sebastiencs/llmobs:0.0.0

# Address the ports are published on. 127.0.0.1 keeps them local (put a TLS reverse proxy in
# front); 0.0.0.0 publishes them on every interface.
#LLMOBS_BIND=127.0.0.1
#LLMOBS_HTTP_PORT=8080
#LLMOBS_OTLP_GRPC_PORT=4317
#LLMOBS_OTLP_HTTP_PORT=4318

# compose.s3.yaml only. Generate each value once with the command next to it.
# openssl rand -hex 32
GARAGE_RPC_SECRET=
# openssl rand -base64 32
GARAGE_ADMIN_TOKEN=
# echo "GK$(openssl rand -hex 12)"
GARAGE_ACCESS_KEY_ID=
# openssl rand -hex 32
GARAGE_SECRET_ACCESS_KEY=
```

`.gitignore` already ignores `.env` and `.env.*` except `.env.example`, so `.env.example` is
tracked and a user's `.env` is not.

### `tools/packaging/compose-smoke.sh` (executable)

```bash
#!/usr/bin/env bash
# Runs deploy/compose with an image and runs smoke.sh against it (D-N-9, D-N-13). With `s3`,
# adds compose.s3.yaml and checks that a flush wrote objects to the Garage bucket.
#
# Usage: compose-smoke.sh <image> [s3]
# Works on a copy of deploy/compose in a temporary directory, so the checkout stays clean.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if [[ $# -lt 1 || $# -gt 2 || ( $# -eq 2 && "$2" != "s3" ) ]]; then
  echo "usage: compose-smoke.sh <image> [s3]" >&2
  exit 2
fi
image="$1"
mode="${2:-local}"
project="llmobs-smoke-$$"
key="sk-llmobs-SMOKE0123456789ABCDEFGHIJKLMNOPQ"

dir="$(mktemp -d)"
cp -R "$root/deploy/compose/." "$dir/"
files=(-f compose.yaml)
if [[ "$mode" == "s3" ]]; then
  files+=(-f compose.s3.yaml)
fi
compose() {
  (cd "$dir" && docker compose -p "$project" "${files[@]}" "$@")
}
cleanup() {
  compose down -v --remove-orphans >/dev/null 2>&1 || true
  rm -rf "$dir"
}
trap cleanup EXIT

read -r http_port grpc_port otlp_port < <(python3 -c '
import socket

socks = [socket.socket() for _ in range(3)]
for sock in socks:
    sock.bind(("127.0.0.1", 0))
print(*(sock.getsockname()[1] for sock in socks))
for sock in socks:
    sock.close()
')

cat >"$dir/.env" <<EOF
LLMOBS_IMAGE=$image
LLMOBS_BIND=127.0.0.1
LLMOBS_HTTP_PORT=$http_port
LLMOBS_OTLP_GRPC_PORT=$grpc_port
LLMOBS_OTLP_HTTP_PORT=$otlp_port
GARAGE_RPC_SECRET=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
GARAGE_ADMIN_TOKEN=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
GARAGE_ACCESS_KEY_ID=GK$(python3 -c 'import secrets; print(secrets.token_hex(12))')
GARAGE_SECRET_ACCESS_KEY=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
EOF
cat >"$dir/llmobs.env" <<EOF
LLMOBS_PUBLIC_URL=http://127.0.0.1:$http_port
LLMOBS_INIT_ADMIN_EMAIL=smoke@example.com
LLMOBS_INIT_ADMIN_PASSWORD=llmobs-smoke-1
LLMOBS_INIT_ORG=Smoke
LLMOBS_INIT_PROJECT=Smoke
LLMOBS_INIT_SECRET_KEY=$key
LLMOBS_STORAGE__FLUSH__MAX_AGE=5s
EOF

# Not `--wait`: garage-init is a one-shot container. smoke.sh waits for /readyz instead.
if ! compose up -d; then
  compose ps -a >&2
  compose logs >&2
  exit 1
fi
if ! "$root/tools/packaging/smoke.sh" "http://127.0.0.1:$http_port" "http://127.0.0.1:$otlp_port" "$key"; then
  compose logs >&2
  exit 1
fi

if [[ "$mode" == "s3" ]]; then
  # The span is flushed to Parquet in the bucket within flush.max_age (5 s here).
  # shellcheck disable=SC2016 # expanded by the container's shell
  check='curl -sSf -H "Authorization: Bearer $GARAGE_ADMIN_TOKEN" "http://garage:3903/v2/GetBucketInfo?globalAlias=llmobs" | tr -d " \n" | grep -q "\"objects\":[1-9]"'
  for _ in $(seq 1 60); do
    if compose run --rm --no-deps --entrypoint /bin/sh garage-init -c "$check" >/dev/null 2>&1; then
      echo "compose-smoke: objects written to the Garage bucket"
      # garage-init must succeed again on an initialized Garage (a restart, a second `up`).
      if ! compose run --rm --no-deps garage-init; then
        compose logs >&2
        echo "compose-smoke: garage-init failed on its second run" >&2
        exit 1
      fi
      echo "compose-smoke: garage-init is idempotent"
      echo "compose-smoke: $image passed ($mode)"
      exit 0
    fi
    sleep 1
  done
  compose logs >&2
  echo "compose-smoke: no object in the Garage bucket after 60 s" >&2
  exit 1
fi
echo "compose-smoke: $image passed ($mode)"
```

### `.gitignore`

After the line `!.env.example`, add:

```text
deploy/compose/llmobs.env
```

### `justfile`

Append after the `image-smoke` recipe:

```just
# Run deploy/compose with an image and smoke-test it; `just compose-smoke llmobs:dev s3` adds Garage (D-N-9)
compose-smoke tag="llmobs:dev" *mode:
    tools/packaging/compose-smoke.sh {{tag}} {{mode}}
```

## Verify

```bash
export PATH="$HOME/.local/share/nvm/v26.13.0/bin:$PATH"
(cd deploy/compose && docker compose config --quiet && docker compose -f compose.yaml -f compose.s3.yaml config --quiet) || true
just image llmobs:dev            # after just dist, as in N4
just compose-smoke llmobs:dev
just compose-smoke llmobs:dev s3
just check
just assert-clean
```

- The two `docker compose config` commands print nothing for `compose.yaml`. For the S3 overlay,
  run from the checkout without a `.env`, they print `set GARAGE_RPC_SECRET in .env` (a
  `:?` guard). That is expected, hence the `|| true`. A YAML error would also show here, so read
  the output.
- `compose-smoke llmobs:dev` ends with `compose-smoke: llmobs:dev passed (local)`.
- `compose-smoke llmobs:dev s3` prints `compose-smoke: objects written to the Garage bucket` and
  ends with `… passed (s3)`. On failure it prints `docker compose logs`. Look for a
  `garage-init:` line first: the init's `curl -f` reports the failing admin call.
- In S3 mode it also prints `garage-init: done` from a second run of the init, then
  `compose-smoke: garage-init is idempotent`.
- `docker volume ls | grep llmobs-smoke` prints nothing afterwards.
- If `up -d` stalls before `garage-init` starts, Garage's healthcheck (`/garage status`) is failing:
  `docker inspect --format '{{json .State.Health.Log}}' <garage container>` shows its output.

If a Garage admin call fails with a 4xx, compare the request body with Garage v2.5.0's admin API
reference (`doc/api/garage-admin-v2.json` in the Garage repository at tag `v2.5.0`). Report the
call and the answer; do not switch the S3 server.

## Done when

- [ ] The seven files exist; `garage-init.sh` and `compose-smoke.sh` are mode `100755`;
      `.gitignore` and the `justfile` are updated.
- [ ] Both compose smokes pass.
- [ ] `just check` and `just assert-clean` pass. One commit: `Add docker-compose files`.
