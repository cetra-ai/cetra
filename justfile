# List recipes
default:
    @just --list

# Format Rust, the web and TypeScript SDK (Biome), and the Python SDK (ruff)
fmt:
    cargo fmt --all
    cargo fmt --manifest-path fuzz/Cargo.toml
    pnpm exec biome format --write .
    cd sdks/python && uv run ruff format

# Check formatting
fmt-check:
    cargo fmt --all -- --check
    cargo fmt --manifest-path fuzz/Cargo.toml -- --check

# Lint (warnings are errors)
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Run all Rust tests
test:
    cargo test --workspace --all-features
    # The crash tests must reuse the workspace feature set: `-p llmobs-storage`
    # resolves different features and rebuilds every dependency from scratch.
    cargo test --workspace --all-features -- --ignored --exact crash_tests::every_flush_hook_aborts_safely
    cargo test --workspace --all-features -- --ignored --exact crash_tests::every_compaction_hook_aborts_safely
    cargo test --workspace --all-features -- --ignored --exact crash_tests::every_deletion_hook_aborts_safely
    cargo test --workspace --all-features -- --ignored --exact crash_tests::every_media_hook_aborts_safely
    cargo test --workspace --all-features -- --ignored --exact crash_tests::every_drop_hook_aborts_safely
    cargo test --workspace --all-features -- --ignored --exact crash_tests::every_flush_hook_survives_a_lossy_crash
    cargo test --workspace --all-features -- --ignored --exact crash_tests::every_compaction_hook_survives_a_lossy_crash
    cargo test --workspace --all-features -- --ignored --exact crash_tests::every_deletion_hook_survives_a_lossy_crash
    cargo test --workspace --all-features -- --ignored --exact crash_tests::every_media_hook_survives_a_lossy_crash
    cargo test --workspace --all-features -- --ignored --exact crash_tests::every_drop_hook_survives_a_lossy_crash
    cargo test --workspace --all-features -- --ignored --exact model_tests::hook_crash::every_model_hook_crash_reads_back
    cargo test --workspace --all-features -- --ignored --exact crash_tests::every_retention_hook_aborts_safely
    cargo test --workspace --all-features -- --ignored --exact crash_tests::a_rotation_crash_reopens_with_the_acknowledged_rows

# The storage model test with more cases (D-R-14), in release mode.
model cases="500":
    LLMOBS_MODEL_CASES={{cases}} cargo test --release --workspace --all-features -- model_tests::storage_matches_the_model

# The concurrent model stress test (track R M6), release mode. `secs` is the
# run time; LLMOBS_STRESS_* variables tune it (docs/plans/r-hardening/06-m6-concurrency.md).
stress secs="300":
    LLMOBS_STRESS_SECS={{secs}} cargo test --release --workspace --all-features -- --ignored --exact --nocapture model_tests::stress::concurrent_lanes_match_the_model

# Resource exhaustion (track R M5, D-R-9): a full disk, a memory limit and few file descriptors.
# Needs unprivileged user namespaces, or LLMOBS_TEST_SMALL_FS (a small mounted filesystem).
# Ubuntu 24.04 and later restrict them with AppArmor: run
# `sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0` first (until the next reboot).
resources:
    cargo test --workspace --all-features -- --ignored --test-threads=1 model_tests::resources::small_fs:: model_tests::resources::disk_full:: model_tests::resources::descriptors::

# Memory-limit runs (track R M5, D-R-9), release mode, under a systemd user scope.
resources-memory:
    cargo test --release --workspace --all-features -- --ignored --test-threads=1 --exact model_tests::resources::memory::defaults_at_the_documented_minimum model_tests::resources::memory::small_profile_at_its_minimum model_tests::resources::memory::child_sees_the_scope_limit

# The smallest memory limit a profile survives (defaults or small). Takes tens of minutes.
resources-memory-min profile="defaults":
    LLMOBS_MEMORY_PROFILE={{profile}} cargo test --release --workspace --all-features -- --ignored --exact --nocapture model_tests::resources::memory::minimum_memory

# The storage model test on a remote store with injected S3 faults, in release mode (R4.5).
model-s3 cases="200":
    LLMOBS_MODEL_S3_CASES={{cases}} LLMOBS_MODEL_S3_STATS=1 cargo test --release --workspace --all-features -- model_tests::storage_matches_the_model_on_s3

# S3 chaos through toxiproxy in front of an S3 endpoint (R M4). Needs
# LLMOBS_TEST_S3_ENDPOINT, LLMOBS_TEST_S3_BUCKET, AWS_ACCESS_KEY_ID,
# AWS_SECRET_ACCESS_KEY and LLMOBS_TEST_TOXIPROXY; see crates/storage/tests/s3_chaos.rs.
s3-chaos:
    cargo test --release --workspace --all-features --test s3_chaos -- --ignored --test-threads 1 --nocapture

# Cross-check the storage crash tests on LazyFS, which drops unsynced data at each crash (R2.12).
# Needs FUSE 3 and `user_allow_other`: tools/lazyfs/README.md. Manual, not in `just test`.
lazyfs:
    tools/lazyfs/lazyfs.sh build && tools/lazyfs/lazyfs.sh run

# Download the pinned text corpus of the storage benchmarks and soak (OpenAssistant oasst2, Apache-2.0; track R M7)
fetch-corpus:
    mkdir -p target/bench-corpus
    curl -fL --retry 3 -o target/bench-corpus/oasst2-ready-messages.jsonl.gz.part https://huggingface.co/datasets/OpenAssistant/oasst2/resolve/179dd21fc55192153d94adb0e0ce8f69e222bf75/2023-11-05_oasst2_ready.messages.jsonl.gz
    echo "a9f240c4c77aa1378364f70d37e753c07ba284e247b019d700e1947a0e5da751  target/bench-corpus/oasst2-ready-messages.jsonl.gz.part" | sha256sum --check --strict -
    mv target/bench-corpus/oasst2-ready-messages.jsonl.gz.part target/bench-corpus/oasst2-ready-messages.jsonl.gz

# Storage soak (D-R-11). Needs LLMOBS_SOAK_DIR and LLMOBS_SOAK_OUT; see crates/storage/tests/soak.rs
soak:
    RUST_MIN_STACK=33554432 cargo test --release -p llmobs-storage --all-features --test soak -- --ignored --nocapture --exact soak

# Plots of a soak run's samples into <out>/plots
soak-plot out:
    cd tools/soak && uv run --locked python plot.py {{absolute_path(out)}}

# Lint and type-check the soak plot script (not part of `just check`)
check-soak-plot:
    cd tools/soak && uv sync --locked && uv run ruff format --check && uv run ruff check && uv run mypy plot.py

# Write the fuzz seed corpus from the test fixtures (track R M3)
fuzz-seed:
    cargo test --workspace --all-features -- --ignored --exact fuzz_entry::tests::write_seed_corpus

# The fuzzing toolchain, updated by hand like rust-toolchain.toml (track R M3)
fuzz_toolchain := "nightly-2026-11-03"
fuzz_targets := "wal_record wal_replay parquet_file read_cache_block catalog_rows filter_dsl otlp_http_protobuf otlp_http_json otlp_grpc"

# Install the fuzzing toolchain and cargo-fuzz (track R M3)
# Install the fuzzing toolchain (track R M3); CI installs cargo-fuzz with taiki-e instead
fuzz-toolchain:
    rustup toolchain install {{fuzz_toolchain}} --profile minimal

# Install the fuzzing toolchain and cargo-fuzz (track R M3)
fuzz-tools: fuzz-toolchain
    cargo install cargo-fuzz --version 0.13.2 --locked

# Write fuzz/Cargo.lock: the root lock's versions, plus the exact libfuzzer-sys pin
fuzz-lock:
    cp Cargo.lock fuzz/Cargo.lock
    cargo metadata --manifest-path fuzz/Cargo.toml --format-version 1 > /dev/null

# Fuzz one target for `secs` seconds; new inputs go into its committed corpus
fuzz target secs="60": fuzz-lock
    cargo +{{fuzz_toolchain}} fuzz run {{target}} fuzz/corpus/{{target}} -- -max_total_time={{secs}} -rss_limit_mb=4096 -timeout=30 -max_len=$(just _fuzz-max-len {{target}})

# Fuzz one target for CI: new inputs go to target/fuzz-new/<target>, so the checkout stays clean
fuzz-ci target secs: fuzz-lock
    mkdir -p target/fuzz-new/{{target}}
    cargo +{{fuzz_toolchain}} fuzz run {{target}} target/fuzz-new/{{target}} fuzz/corpus/{{target}} -- -max_total_time={{secs}} -rss_limit_mb=4096 -timeout=30 -max_len=$(just _fuzz-max-len {{target}})

# Fuzz every target in turn; fails if any target crashed
fuzz-all secs="60":
    #!/usr/bin/env bash
    set -uo pipefail
    rc=0
    for target in {{fuzz_targets}}; do just fuzz "$target" {{secs}} || rc=1; done
    exit "$rc"

# Minimize every corpus, then fit the 2 MiB and file-count budgets by dropping the largest files
fuzz-cmin: fuzz-lock
    #!/usr/bin/env bash
    set -euo pipefail
    for target in {{fuzz_targets}}; do
        dir="fuzz/corpus/$target"
        cargo +{{fuzz_toolchain}} fuzz cmin "$target" "$dir" -- -max_len="$(just _fuzz-max-len "$target")"
        dropped=0
        while [ "$(du -sb "$dir" | cut -f1)" -gt 2097152 ]; do
            rm -- "$dir/$(ls -S "$dir" | head -n 1)"
            dropped=$((dropped + 1))
        done
        max_files="$(just _fuzz-max-files "$target")"
        while [ "$(find "$dir" -type f | wc -l)" -gt "$max_files" ]; do
            rm -- "$dir/$(ls -S "$dir" | head -n 1)"
            dropped=$((dropped + 1))
        done
        if [ "$dropped" -gt 0 ]; then
            echo "$target: dropped $dropped largest files to fit the budgets" >&2
        fi
    done

# The largest input a target takes, in bytes (libFuzzer's default is too small for Parquet)
[private]
_fuzz-max-len target:
    #!/usr/bin/env bash
    case "{{target}}" in
        parquet_file) echo 1048576 ;;
        wal_replay|otlp_http_protobuf|otlp_http_json|otlp_grpc) echo 262144 ;;
        *) echo 65536 ;;
    esac

# The file-count budget of a target's corpus, so its stable replay stays under 10 s in debug
[private]
_fuzz-max-files target:
    #!/usr/bin/env bash
    case "{{target}}" in
        parquet_file|filter_dsl|catalog_rows) echo 300 ;;
        *) echo 2000 ;;
    esac

# Open or update the one open `nightly` issue for a failed nightly run (RUN_URL from the environment)
nightly-issue:
    #!/usr/bin/env bash
    set -euo pipefail
    gh label create nightly --force --description "A nightly run failed"
    number="$(gh issue list --label nightly --state open --json number --jq '.[0].number // empty')"
    if [ -n "$number" ]; then
        gh issue comment "$number" --body "Nightly failed again: $RUN_URL"
    else
        gh issue create --title "Nightly run failed" --label nightly --body "Nightly run failed: $RUN_URL"
    fi

# Opt-in live provider tests. Ignored by `just check` (D167).
eval-live:
    cargo test -p llmobs-eval --test live -- --ignored --test-threads 1

# Supply-chain checks: advisories, bans, licences, sources (D-P-3), for the root graph and the fuzz graph (track R M3)
deny: fuzz-lock
    cargo deny check advisories bans licenses sources
    cargo deny --manifest-path fuzz/Cargo.toml check advisories bans licenses sources

# Debug build
build:
    cargo build --workspace

# Build the web UI, then the release binary with the UI embedded (D-G-18)
build-release: js-install
    pnpm --dir web run build
    cargo build --release -p llmobs-cli

# Build the release binary and check that the UI is embedded (D-G-37)
check-release: build-release
    ./target/release/llmobs version | grep -qx 'ui: embedded'

# Run the binary with arguments, e.g. `just run --version`
run *args:
    cargo run -p llmobs-cli -- {{args}}

# Rust checks: format, lint, tests, supply chain
check-rust: fmt-check lint test deny check-ee

# Install JS dependencies from the lockfile
js-install:
    pnpm install --frozen-lockfile

# Web UI checks: lint, types, tests, production build
check-web: js-install
    pnpm --dir web run check

# Python SDK checks: format, lint, types, tests
check-py:
    cd sdks/python && uv sync --locked && uv run ruff format --check && uv run ruff check && uv run mypy && uv run pytest

# TypeScript SDK checks: lint, types, tests, package
check-ts: js-install
    pnpm --dir sdks/typescript run check

# Every check (D79); must pass before every commit
check: check-rust check-web check-py check-ts

# Build the web, run the Rust e2e tests with the UI embedded, then the SDK suites against a real server (D-G-20, D-G-18)
e2e: js-install
    pnpm --dir web run build
    E2E_REQUIRE_UI=1 cargo test --workspace --all-features --test e2e
    bash tools/e2e/run.sh
    ./target/debug/llmobs version | grep -qx 'ui: embedded'

# Fail if the working tree changed (generated files, snapshot or fixture writes)
assert-clean:
    git status --porcelain --untracked-files=all
    git diff --exit-code
    test -z "$(git status --porcelain --untracked-files=all)"

# Install a packed TypeScript SDK into a scratch project and load it from ESM and CommonJS
ts-smoke tarball:
    #!/usr/bin/env bash
    set -euo pipefail
    tarball="$(realpath '{{tarball}}')"
    expected="$(tar -xOzf "$tarball" package/package.json | node -p 'JSON.parse(require("fs").readFileSync(0, "utf8")).version')"
    dir="$(mktemp -d)"
    trap 'rm -rf "$dir"' EXIT
    cd "$dir"
    npm init -y >/dev/null
    # The SDK's dependencies come from npm, with no lockfile. Hold them to the 7-day age gate that
    # pnpm's minimumReleaseAge applies to the repo (D74); npm 10, shipped with Node 22.12, supports --before.
    before="$(node -p 'new Date(Date.now() - 7*24*3600*1000).toISOString()')"
    npm install --no-audit --no-fund --ignore-scripts --before "$before" "$tarball" >/dev/null
    EXPECTED="$expected" node --input-type=module -e 'import { VERSION } from "llmobs"; if (VERSION !== process.env.EXPECTED) { console.error(`ESM: VERSION ${VERSION}`); process.exit(1); }'
    EXPECTED="$expected" node -e 'const { VERSION } = require("llmobs"); if (VERSION !== process.env.EXPECTED) { console.error(`CJS: VERSION ${VERSION}`); process.exit(1); }'
    echo "llmobs ${expected} loads from ESM and CommonJS on Node $(node --version)"

# Lint GitHub Actions workflows (actionlint) and audit them for security issues (zizmor)
check-workflows:
    actionlint
    zizmor --offline .github

# RustSec advisories only; run weekly by CI
deny-advisories: fuzz-lock
    cargo deny check advisories
    cargo deny --manifest-path fuzz/Cargo.toml check advisories

# Check the ee licence files and SPDX headers (D-P-2)
check-ee:
    #!/usr/bin/env bash
    set -euo pipefail
    expected=6448026f2e06ab3da9ed8ddda686694bba37179e88db92dea6d4aab516c957ea
    header='// SPDX-License-Identifier: Elastic-2.0'
    status=0
    for dir in crates/server/src/ee web/src/ee tools/license crates/token; do
        if [ ! -f "$dir/LICENSE" ]; then
            echo "$dir/LICENSE is missing"
            status=1
        elif [ "$(sha256sum "$dir/LICENSE" | cut -d' ' -f1)" != "$expected" ]; then
            echo "$dir/LICENSE is not the Elastic License 2.0 text"
            status=1
        fi
        while IFS= read -r -d '' file; do
            if [ "$(head -n 1 "$file")" != "$header" ]; then
                echo "$file: the first line must be '$header'"
                status=1
            fi
        done < <(find "$dir" -type f \( -name '*.rs' -o -name '*.ts' -o -name '*.tsx' \) -print0)
    done
    while IFS= read -r file; do
        case "$file" in
            crates/server/src/ee/*|web/src/ee/*|tools/license/*|crates/token/*) ;;
            *) echo "$file: Elastic-2.0 header outside an ee directory"; status=1 ;;
        esac
    done < <(git grep -l -F 'SPDX-License-Identifier: Elastic-2.0' -- '*.rs' '*.ts' '*.tsx' || true)
    exit "$status"

# Re-sign fixtures/license/payloads with the dev key (D-P-6)
license-fixtures:
    cargo run -q -p llmobs-license-tool -- fixtures

# Web dev server with mocked API (fixtures/api)
web-dev: js-install
    pnpm --dir web run dev

# Web dev server proxied at the real API (no mocks)
web-dev-server: js-install
    pnpm --dir web run dev:server

# Regenerate crates/server/openapi.json and web/src/api/schema.gen.ts
openapi: js-install
    LLMOBS_OPENAPI_WRITE=1 cargo test -p llmobs-server --test openapi -- contract_file_is_up_to_date
    pnpm --dir web run gen:api

# Regenerate fixtures/api responses and manifest
api-examples:
    LLMOBS_API_EXAMPLES_WRITE=1 cargo test -p llmobs-server --lib api_examples::examples_are_up_to_date

# Validate OTLP fixtures
fixtures-check:
    cargo test -p llmobs-otel --test fixtures

# Rewrite OTLP fixtures in canonical JSON formatting
fixtures-fmt:
    LLMOBS_FIXTURES_FMT=1 cargo test -p llmobs-otel --test fixtures -- canonical_formatting

# Rewrite expected.json from the normalizer. Review the diff before committing.
fixtures-bless:
    LLMOBS_BLESS=1 cargo test -p llmobs-otel --test golden

# Regenerate the built-in price snapshot from a downloaded LiteLLM JSON (D104).
# Download model_prices_and_context_window.json from
# https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json
# by hand. This recipe does not fetch it.
prices-update litellm_json:
    LLMOBS_LITELLM_JSON={{litellm_json}} cargo test -p llmobs-otel --test prices -- refresh

# Fixture capture

# Capture OTLP fixtures from real instrumentations (tools/capture), e.g. `just capture openinference/openai-chat-tool`
capture +cases: capture-py-sync capture-ts-sync
    cargo run -q -p llmobs-capture -- run {{cases}}

# Recapture every scenario
capture-all: capture-py-sync capture-ts-sync
    cargo run -q -p llmobs-capture -- run --all

# Install the Python capture project (tools/capture/py)
capture-py-sync:
    cd tools/capture/py && uv sync --locked

# Install the TypeScript capture project (tools/capture/ts)
capture-ts-sync:
    pnpm --dir tools/capture/ts install --frozen-lockfile

# Lint and type-check the capture scenarios, then recapture every scenario and compare (timing excluded)
check-capture: capture-py-sync capture-ts-sync
    cd tools/capture/py && uv run ruff format --check && uv run ruff check && uv run mypy
    pnpm --dir tools/capture/ts run typecheck
    pnpm exec biome check tools/capture/ts
    cargo run -q -p llmobs-capture -- run --all --check

# Packaging and release (track N, docs/plans/n-packaging/)

# Write target/dist/THIRD-PARTY-NOTICES.md: licences of the crates and web packages in the binary (D-N-8)
notices: js-install
    tools/packaging/notices.sh

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

# Build the image for this machine's architecture from target/dist/image (run `just dist` first) (D-N-3)
image tag="llmobs:dev":
    docker build -f deploy/docker/Dockerfile -t {{tag}} .

# Run an image with a fresh volume and smoke-test it, healthcheck included (D-N-13)
image-smoke tag="llmobs:dev":
    tools/packaging/image-smoke.sh {{tag}}

# Run deploy/install.sh against target/dist served on loopback (D-N-15)
install-smoke:
    tools/packaging/install-smoke.sh

# Run deploy/compose with an image and smoke-test it; `just compose-smoke llmobs:dev s3` adds Garage (D-N-9)
compose-smoke tag="llmobs:dev" *mode:
    tools/packaging/compose-smoke.sh {{tag}} {{mode}}

# Lint the Helm chart and validate its manifests with kubeconform (D-N-10)
helm-check:
    tools/packaging/helm-check.sh

# Install the chart with an image on a throwaway kind cluster and smoke-test it (D-N-10)
helm-smoke tag="llmobs:dev":
    tools/packaging/helm-smoke.sh {{tag}}

# Check that the chart and compose files carry the workspace version (D-N-5)
versions-check:
    tools/packaging/versions-check.sh

# Set the release version everywhere and date the changelog section, e.g. `just release-prep 0.1.0` (D-N-5)
release-prep version:
    tools/packaging/release-prep.sh {{version}}

# Release workflow gate: tag = version, files agree, changelog section, tag on origin/main (D-N-5)
release-verify tag:
    tools/packaging/release-verify.sh {{tag}}

# Print a version's changelog section (the GitHub release notes)
release-notes version:
    tools/packaging/release-notes.sh {{version}}

# Cheap packaging checks: versions, chart, compose files, Biome on tools/packaging (CI's packaging job, D-N-13)
check-packaging-lint: versions-check helm-check js-install
    pnpm exec biome check tools/packaging
    cd deploy/compose && docker compose config --quiet
    cd deploy/compose && GARAGE_RPC_SECRET=x GARAGE_ADMIN_TOKEN=x GARAGE_ACCESS_KEY_ID=x GARAGE_SECRET_ACCESS_KEY=x docker compose -f compose.yaml -f compose.s3.yaml config --quiet

# Full packaging checks on x86_64 Linux: the lint checks, the archive, binary, install, image and compose smokes (packaging.yml, D-N-13)
check-packaging: check-packaging-lint
    just dist x86_64-unknown-linux-musl
    just binary-smoke target/x86_64-unknown-linux-musl/release/llmobs
    just install-smoke
    just image llmobs:dev
    just image-smoke llmobs:dev
    just compose-smoke llmobs:dev
    just compose-smoke llmobs:dev s3
