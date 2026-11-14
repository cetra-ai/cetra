# N3 — Release archives and the smoke test

Read [README.md](README.md) first, especially § Design › Build, § Design › Verification and
§ Conventions.

**Goal:**
- `just dist <target>` builds the release archive `target/dist/llmobs-<version>-<target>.tar.gz`
  for `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` or `aarch64-apple-darwin`
  (D-N-1, D-N-16). For a Linux target it also stages the image build context
  `target/dist/image/linux-<amd64|arm64>/` that N4's Dockerfile copies.
- `just binary-smoke <binary>` starts a binary and runs the smoke test that every later launcher
  reuses (D-N-13).
- `just checksums <dir>` writes `SHA256SUMS` (D-N-7).

**Prerequisite:** P's P1 is on `main`. `test -f LICENSE && test -f crates/server/src/ee/LICENSE`
must succeed; if not, stop and report. `dist.sh` checks this too.

**Edit only:**
- `tools/packaging/version.sh`, `tools/packaging/dist.sh`, `tools/packaging/image-context.sh`,
  `tools/packaging/smoke.sh`, `tools/packaging/binary-smoke.sh`, `tools/packaging/checksums.sh`
  (all new, executable);
- `justfile`.

**Commit:** `Add release archives and the smoke test`

**Tools:** musl-gcc (`musl-gcc --version`), cargo-auditable 0.7.7 (`cargo auditable --version`),
cargo-about 0.9.3, Node on `PATH`, python3, curl, `file`.

## How it works

`dist.sh <target>`:
1. Checks that the licence files exist.
2. Reads the version (`version.sh`).
3. `rustup target add <target>`.
4. Builds the web.
5. `cargo auditable build --release --locked --target <target> -p llmobs-cli`. For musl it sets
   `CC_<target>=musl-gcc` so the C dependencies compile against musl.
6. Runs the binary, which must print `llmobs <version>` and `ui: embedded`. A Linux binary must
   also be static (`file` says `static`).
7. Writes the notices (N2).
8. Stages `target/dist/llmobs-<version>-<target>/` with `llmobs`, `LICENSE`, `LICENSE-ELASTIC` and
   `THIRD-PARTY-NOTICES.md`, and tars it.
9. On Linux, calls `image-context.sh` on the archive.

`image-context.sh` stages the image context from the archive rather than from
`target/<target>/release`. The release workflow (N10) then rebuilds the context from downloaded
archives: artifact uploads drop file modes and empty directories, and tar keeps them.

`dist.sh` must run on a host of the target's OS and architecture: it executes the binary. There is
no cross-building.

The smoke test and its launcher are described in README § Design › Verification. The fixed key
`sk-llmobs-SMOKE0123456789ABCDEFGHIJKLMNOPQ` has 32 characters after the prefix (spec 08).

## Files

Create each file with exactly this content, then `chmod +x` it.

### `tools/packaging/version.sh`

```bash
#!/usr/bin/env bash
# Prints the workspace version, `version` in `[workspace.package]` of the root Cargo.toml (D-N-5).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
version="$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version = "\(.*\)"$/\1/p;}' "$root/Cargo.toml")"
if [[ -z "$version" ]]; then
  echo "version.sh: no version in [workspace.package] of Cargo.toml" >&2
  exit 1
fi
echo "$version"
```

### `tools/packaging/dist.sh`

```bash
#!/usr/bin/env bash
# Builds the release archive of one target into target/dist (D-N-1, D-N-16):
#
#   target/dist/llmobs-<version>-<target>.tar.gz
#     llmobs-<version>-<target>/llmobs
#     llmobs-<version>-<target>/LICENSE                  the root LICENSE (MIT, names the ee dirs)
#     llmobs-<version>-<target>/LICENSE-ELASTIC          crates/server/src/ee/LICENSE (ELv2)
#     llmobs-<version>-<target>/THIRD-PARTY-NOTICES.md
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
name="llmobs-${version}-${target}"

rustup target add "$target"
pnpm --dir web run build
cargo auditable build --release --locked --target "$target" -p llmobs-cli
bin="target/${target}/release/llmobs"

# The binary must run here and embed the UI (D-G-37).
"$bin" version | grep -qx 'ui: embedded'
"$bin" version | grep -qx "llmobs ${version}"
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
install -m 0755 "$bin" "$stage/llmobs"
install -m 0644 LICENSE "$stage/LICENSE"
install -m 0644 crates/server/src/ee/LICENSE "$stage/LICENSE-ELASTIC"
install -m 0644 target/dist/THIRD-PARTY-NOTICES.md "$stage/THIRD-PARTY-NOTICES.md"
tar -czf "target/dist/${name}.tar.gz" -C target/dist "$name"
echo "wrote target/dist/${name}.tar.gz"

if [[ -n "$arch" ]]; then
  tools/packaging/image-context.sh "target/dist/${name}.tar.gz" "$arch"
fi
```

### `tools/packaging/image-context.sh`

```bash
#!/usr/bin/env bash
# Stages the Docker build context of one architecture from a Linux release archive (D-N-3):
#
#   target/dist/image/linux-<arch>/rootfs/usr/local/bin/llmobs                    (mode 0755)
#   target/dist/image/linux-<arch>/rootfs/usr/share/doc/llmobs/{LICENSE,LICENSE-ELASTIC,THIRD-PARTY-NOTICES.md}
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
mkdir -p "$context/rootfs/usr/local/bin" "$context/rootfs/usr/share/doc/llmobs" "$context/datadir/data"
install -m 0755 "$tmp/$name/llmobs" "$context/rootfs/usr/local/bin/llmobs"
install -m 0644 "$tmp/$name/LICENSE" "$tmp/$name/LICENSE-ELASTIC" "$tmp/$name/THIRD-PARTY-NOTICES.md" \
  "$context/rootfs/usr/share/doc/llmobs/"
echo "wrote target/dist/image/linux-$arch"
```

### `tools/packaging/smoke.sh`

```bash
#!/usr/bin/env bash
# Smoke-tests a running llmobs (D-N-13): readiness, the embedded UI, and one span sent over
# OTLP/HTTP and read back through the API.
#
# Usage: smoke.sh <http_url> <otlp_http_url> <secret_key>
#   http_url       the UI and API listener, e.g. http://127.0.0.1:8080
#   otlp_http_url  the OTLP/HTTP listener, e.g. http://127.0.0.1:4318
#   secret_key     an `sk-llmobs-…` key of the project (LLMOBS_INIT_SECRET_KEY)
#
# Needs bash, curl and python3. Waits up to 120 s for /readyz.
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: smoke.sh <http_url> <otlp_http_url> <secret_key>" >&2
  exit 2
fi
base="${1%/}"
otlp="${2%/}"
key="$3"

ready=0
for _ in $(seq 1 120); do
  if curl -sf "$base/readyz" >/dev/null; then
    ready=1
    break
  fi
  sleep 1
done
if [[ "$ready" -ne 1 ]]; then
  echo "smoke: $base/readyz did not answer 200 within 120 s" >&2
  exit 1
fi
echo "smoke: $base/readyz is ready"

# The embedded UI (D-G-18): the built index.html loads hashed bundles from /assets/.
if ! curl -sf -H 'Accept: text/html' "$base/" | grep -q '/assets/'; then
  echo "smoke: $base/ is not the built web UI" >&2
  exit 1
fi
echo "smoke: the web UI is embedded"

trace_id="$(python3 -c 'import secrets; print(secrets.token_hex(16))')"
span_id="$(python3 -c 'import secrets; print(secrets.token_hex(8))')"
start="$(python3 -c 'import time; print(time.time_ns() - 1_000_000_000)')"
end="$(python3 -c 'import time; print(time.time_ns())')"
body="$(cat <<EOF
{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"llmobs-smoke"}}]},"scopeSpans":[{"scope":{"name":"llmobs-smoke"},"spans":[{"traceId":"$trace_id","spanId":"$span_id","name":"llmobs-smoke","kind":1,"startTimeUnixNano":"$start","endTimeUnixNano":"$end"}]}]}]}
EOF
)"

status="$(curl -s -o /dev/null -w '%{http_code}' -X POST "$otlp/v1/traces" \
  -H "Authorization: Bearer $key" \
  -H 'Content-Type: application/json' \
  --data "$body" || true)"
if [[ "$status" != "200" ]]; then
  echo "smoke: POST $otlp/v1/traces answered $status" >&2
  exit 1
fi
echo "smoke: span sent to $otlp/v1/traces"

# Spans are queryable within seconds (WAL + memtable); poll for 30 s.
for _ in $(seq 1 60); do
  if curl -sf -H "Authorization: Bearer $key" \
    "$base/api/v1/projects/current/traces/$trace_id" 2>/dev/null |
    python3 -c 'import json, sys; sys.exit(0 if json.load(sys.stdin)["trace"]["name"] == "llmobs-smoke" else 1)' 2>/dev/null; then
    echo "smoke: trace $trace_id read back"
    exit 0
  fi
  sleep 0.5
done
echo "smoke: trace $trace_id was not readable within 30 s" >&2
exit 1
```

### `tools/packaging/binary-smoke.sh`

```bash
#!/usr/bin/env bash
# Starts `<binary> serve` on free loopback ports with a fresh data directory and runs smoke.sh
# against it, then `llmobs health` (D-N-4, D-N-13).
#
# Usage: binary-smoke.sh <path to the llmobs binary>
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if [[ $# -ne 1 ]]; then
  echo "usage: binary-smoke.sh <binary>" >&2
  exit 2
fi
bin="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"

# Three ports, held open together so no two are the same, then released for the server.
read -r http_port otlp_port grpc_port < <(python3 -c '
import socket

socks = [socket.socket() for _ in range(3)]
for sock in socks:
    sock.bind(("127.0.0.1", 0))
print(*(sock.getsockname()[1] for sock in socks))
for sock in socks:
    sock.close()
')

dir="$(mktemp -d)"
server_pid=""
cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    for _ in $(seq 20); do
      kill -0 "$server_pid" 2>/dev/null || break
      sleep 0.5
    done
    kill -KILL "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$dir"
}
trap cleanup EXIT

key="sk-llmobs-SMOKE0123456789ABCDEFGHIJKLMNOPQ"

LLMOBS_DATA_DIR="$dir/data" \
LLMOBS_PUBLIC_URL="http://127.0.0.1:${http_port}" \
LLMOBS_HTTP_ADDR="127.0.0.1:${http_port}" \
LLMOBS_OTLP_HTTP_ADDR="127.0.0.1:${otlp_port}" \
LLMOBS_GRPC_ADDR="127.0.0.1:${grpc_port}" \
LLMOBS_INIT_ADMIN_EMAIL="smoke@example.com" \
LLMOBS_INIT_ADMIN_PASSWORD="llmobs-smoke-1" \
LLMOBS_INIT_ORG="Smoke" \
LLMOBS_INIT_PROJECT="Smoke" \
LLMOBS_INIT_SECRET_KEY="$key" \
  "$bin" serve >"$dir/serve.log" 2>&1 &
server_pid=$!

if ! "$root/tools/packaging/smoke.sh" "http://127.0.0.1:${http_port}" "http://127.0.0.1:${otlp_port}" "$key"; then
  cat "$dir/serve.log" >&2
  exit 1
fi
"$bin" health --url "http://127.0.0.1:${http_port}"
echo "binary-smoke: $bin passed"
```

### `tools/packaging/checksums.sh`

```bash
#!/usr/bin/env bash
# Writes <dir>/SHA256SUMS for every .tar.gz in <dir>, in `sha256sum` format (D-N-7).
#
# Usage: checksums.sh <dir>
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: checksums.sh <dir>" >&2
  exit 2
fi
cd "$1"
shopt -s nullglob
files=(*.tar.gz)
if (( ${#files[@]} == 0 )); then
  echo "checksums.sh: no .tar.gz in $1" >&2
  exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "${files[@]}" >SHA256SUMS
else
  shasum -a 256 "${files[@]}" >SHA256SUMS
fi
echo "wrote $1/SHA256SUMS (${#files[@]} files)"
```

### `justfile`

Append after the `notices` recipe (one empty line between recipes):

```just
# Build the release archive of one target into target/dist; Linux targets also stage the image context (D-N-1, D-N-16)
dist target: js-install
    tools/packaging/dist.sh {{target}}

# Stage target/dist/image/linux-<arch> from a Linux release archive (D-N-3)
image-context archive arch:
    tools/packaging/image-context.sh {{archive}} {{arch}}

# Start a binary on free ports and smoke-test it: /readyz, the UI, one OTLP span read back (D-N-13)
binary-smoke binary:
    tools/packaging/binary-smoke.sh {{binary}}

# Write SHA256SUMS for the archives in a directory (D-N-7)
checksums dir:
    tools/packaging/checksums.sh {{dir}}
```

Keep that order: `dist`, `image-context`, `binary-smoke`, `checksums`.

## Verify

```bash
export PATH="$HOME/.local/share/nvm/v26.13.0/bin:$PATH"
just dist x86_64-unknown-linux-musl
tar -tzf target/dist/llmobs-0.0.0-x86_64-unknown-linux-musl.tar.gz
ls -l target/dist/image/linux-amd64/rootfs/usr/local/bin/llmobs target/dist/image/linux-amd64/rootfs/usr/share/doc/llmobs
test -d target/dist/image/linux-amd64/datadir/data
just binary-smoke target/x86_64-unknown-linux-musl/release/llmobs
just checksums target/dist
cat target/dist/SHA256SUMS
just check
just assert-clean
```

Expected:
- `tar -tzf` lists exactly `llmobs-0.0.0-x86_64-unknown-linux-musl/` and its four files.
- The staged binary is `-rwxr-xr-x`.
- `binary-smoke` prints, in order:
  - `smoke: … /readyz is ready`;
  - `smoke: the web UI is embedded`;
  - `smoke: span sent to …`;
  - `smoke: trace <id> read back`;
  - `binary-smoke: … passed`.
- `SHA256SUMS` has one line per archive in `target/dist`.

The first musl release build takes 10–20 minutes. If `cargo auditable build` fails while compiling
a C dependency for musl (a `*-sys` crate), stop and report the crate and the compiler error. Do not
change compilers, add `-I` flags or switch to another build tool.

If `binary-smoke` fails, it prints the server log. A smoke test that times out on
`/readyz` usually means a port clash or a bad init variable; read the log before changing anything.

## Done when

- [ ] The six scripts exist with mode `100755`; the four recipes are in the `justfile`.
- [ ] The commands above pass on x86_64 Linux.
- [ ] `just check` and `just assert-clean` pass. One commit:
      `Add release archives and the smoke test`.
