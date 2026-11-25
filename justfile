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
    # `--all-targets` skips the storage `test = false` targets (benchmarks, S3 probes); name them.
    cargo clippy --workspace --all-targets --all-features --test ingest_bench --test trace_bench --test s3_chaos --test s3_probe -- -D warnings

# The ignored tests that `just test` runs too, as a nextest filterset: the storage crash harness
# (each crash test re-runs its own test binary as the child it kills).
crash_tests := "test(=crash_tests::every_flush_hook_aborts_safely) | test(=crash_tests::every_compaction_hook_aborts_safely) | test(=crash_tests::every_deletion_hook_aborts_safely) | test(=crash_tests::every_media_hook_aborts_safely) | test(=crash_tests::every_drop_hook_aborts_safely) | test(=crash_tests::every_flush_hook_survives_a_lossy_crash) | test(=crash_tests::every_compaction_hook_survives_a_lossy_crash) | test(=crash_tests::every_deletion_hook_survives_a_lossy_crash) | test(=crash_tests::every_media_hook_survives_a_lossy_crash) | test(=crash_tests::every_drop_hook_survives_a_lossy_crash) | test(=model_tests::hook_crash::every_model_hook_crash_reads_back) | test(=crash_tests::every_retention_hook_aborts_safely) | test(=crash_tests::a_rotation_crash_reopens_with_the_acknowledged_rows)"

# Run all Rust tests: cargo-nextest runs the unit and integration tests and the crash harness,
# cargo test the doctests
test: (test-nextest "") test-crash test-doc

# The unit and integration tests with nextest. `partition` is empty for every test, or one CI
# shard as a nextest slice, e.g. `slice:2/4`. The Rust test recipes use the workspace feature
# set: `-p cetra-storage` resolves different features and rebuilds every dependency.
test-nextest partition:
    cargo nextest run --workspace --all-features {{ if partition == "" { "" } else { "--partition " + partition } }}

# The storage crash harness (ignored tests, run in parallel)
test-crash:
    cargo nextest run --workspace --all-features --run-ignored only -E '{{crash_tests}}'

# The doctests (nextest does not run them); CI runs them in rust-lint
test-doc:
    cargo test --workspace --all-features --doc

# One shard of `just test` in CI (`shard` of `total`, .github/CI.md § Rust tests): a slice of
# the unit and integration tests; the last shard also runs the crash harness. The doctests run
# in rust-lint.
test-shard shard total: (test-nextest "slice:" + shard + "/" + total)
    {{ if shard == total { "just test-crash" } else { "true" } }}

# Upload the build's compiled crates to the kache remote now, failing on any failed upload. CI
# runs it on main pushes: kache-action's own upload only warns, so a full bucket would go unseen
# (.github/CI.md § Cache cleanup).
cache-push:
    kache sync --push

# The storage model test with more cases (D-R-14), in release mode.
model cases="500":
    CETRA_MODEL_CASES={{cases}} cargo test --release --workspace --all-features -- model_tests::storage_matches_the_model

# The concurrent model stress test (track R M6), release mode. `secs` is the
# run time; CETRA_STRESS_* variables tune it (docs/plans/r-hardening/06-m6-concurrency.md).
stress secs="300":
    CETRA_STRESS_SECS={{secs}} cargo test --release --workspace --all-features -- --ignored --exact --nocapture model_tests::stress::concurrent_lanes_match_the_model

# Write a golden data directory from the current build (track R M8, D-R-29) and a golden server
# backup directory (track SF6, D-R-38): `just golden-dir 0.1.0` at a release; `just golden-dir
# scratch` to look at both.
golden-dir name:
    #!/usr/bin/env bash
    set -euo pipefail
    root="{{justfile_directory()}}"
    name="{{name}}"
    # A look (scratch, scratch-anything, no dot or slash) or a release version of digits only. An
    # rc tag gets no directory (spec 03 § 12, R8.8).
    if ! [[ "$name" =~ ^(scratch[A-Za-z0-9_-]*|[0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
        echo "golden-dir: $name is not a golden directory name: use scratch (or scratch-<suffix>) for a look, or a release version X.Y.Z with digits only (no pre-release suffix, no / or ..)" >&2; exit 1
    fi
    out="$root/fixtures/storage/golden/$name"
    server_out="$root/fixtures/server/golden/$name"
    for dir in "$out" "$server_out"; do
        if [ -e "$dir" ]; then
            echo "golden-dir: $dir exists; a released directory is never rewritten. If it is a partial write that is not in RELEASES, delete it and run again." >&2; exit 1
        fi
    done
    CETRA_GOLDEN_OUT="$out" cargo test --workspace --all-features -- --ignored --exact golden_tests::write_golden_dir
    CETRA_SERVER_GOLDEN_OUT="$server_out" cargo test -p cetra-server --all-features -- --ignored --exact golden_tests::write_server_golden_dir
    case "$name" in
        scratch*) ;;
        *) echo "$name" >> "$root/fixtures/storage/golden/RELEASES" ;;
    esac

# Fetch main into origin/main with its full history, so check-golden finds the merge base (CI's rust-policy job).
fetch-main:
    git fetch --no-tags origin +refs/heads/main:refs/remotes/origin/main

# Released golden directories and RELEASES lines are never removed (spec 03 § 12, D-R-30).
# Skipped without origin/main. CETRA_GOLDEN_DROP=1 allows the clean-up at 1.0 (D-R-12).
check-golden:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! git rev-parse --verify -q origin/main >/dev/null; then echo "check-golden: no origin/main, skipped"; exit 0; fi
    if [ "${CETRA_GOLDEN_DROP:-}" = 1 ]; then exit 0; fi
    base="$(git merge-base origin/main HEAD)" || { echo "check-golden: no merge base with origin/main (shallow clone?), skipped"; exit 0; }
    gone="$(git diff --no-renames --name-only --diff-filter=D "$base" -- fixtures/storage/golden fixtures/server/golden || true)"
    lines="$(git diff --no-renames --text "$base" -- fixtures/storage/golden/RELEASES | grep -E '^-[^-]' || true)"
    if [ -n "$gone$lines" ]; then
        echo "check-golden: released golden directories must stay (D-R-12):" >&2
        echo "$gone$lines" >&2; exit 1
    fi

# Resource exhaustion (track R M5, D-R-9): a full disk, a memory limit and few file descriptors.
# Needs unprivileged user namespaces, or CETRA_TEST_SMALL_FS (a small mounted filesystem).
# Ubuntu 24.04 and later restrict them with AppArmor: run
# `sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0` first (until the next reboot).
resources:
    cargo test --workspace --all-features -- --ignored --test-threads=1 model_tests::resources::small_fs:: model_tests::resources::disk_full:: model_tests::resources::descriptors::

# Memory-limit runs (track R M5, D-R-9), release mode, under a systemd user scope.
resources-memory:
    cargo test --release --workspace --all-features -- --ignored --test-threads=1 --exact model_tests::resources::memory::defaults_at_the_documented_minimum model_tests::resources::memory::small_profile_at_its_minimum model_tests::resources::memory::child_sees_the_scope_limit

# The smallest memory limit a profile survives (defaults or small). Takes tens of minutes.
resources-memory-min profile="defaults":
    CETRA_MEMORY_PROFILE={{profile}} cargo test --release --workspace --all-features -- --ignored --exact --nocapture model_tests::resources::memory::minimum_memory

# The storage model test on a remote store with injected S3 faults, in release mode (R4.5).
model-s3 cases="200":
    CETRA_MODEL_S3_CASES={{cases}} CETRA_MODEL_S3_STATS=1 cargo test --release --workspace --all-features -- model_tests::storage_matches_the_model_on_s3

# S3 chaos through toxiproxy in front of an S3 endpoint (R M4). Needs
# CETRA_TEST_S3_ENDPOINT, CETRA_TEST_S3_BUCKET, AWS_ACCESS_KEY_ID,
# AWS_SECRET_ACCESS_KEY and CETRA_TEST_TOXIPROXY; see crates/storage/tests/s3_chaos.rs.
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

# Storage soak (D-R-11). Needs CETRA_SOAK_DIR and CETRA_SOAK_OUT; see crates/storage/tests/it/soak.rs
soak:
    RUST_MIN_STACK=33554432 cargo test --release -p cetra-storage --all-features --test it -- --ignored --nocapture --exact soak::soak

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
fuzz_targets := "wal_record wal_replay parquet_file read_cache_block catalog_rows filter_dsl otlp_http_protobuf otlp_http_json otlp_grpc saml_response saml_metadata saml_assertion"

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

# The host triple. cargo-fuzz defaults to the target it was itself built for, so a prebuilt
# (musl) cargo-fuzz would build for musl, which the sanitizer rejects.
_fuzz-host:
    @rustc -vV | sed -n 's/^host: //p'

# Fuzz one target for `secs` seconds; new inputs go into its committed corpus
fuzz target secs="60": fuzz-lock
    cargo +{{fuzz_toolchain}} fuzz run --target "$(just _fuzz-host)" {{target}} fuzz/corpus/{{target}} -- -max_total_time={{secs}} -rss_limit_mb=4096 -timeout=30 -max_len=$(just _fuzz-max-len {{target}})

# Fuzz one target for CI: new inputs go to target/fuzz-new/<target>, so the checkout stays clean
fuzz-ci target secs: fuzz-lock
    mkdir -p target/fuzz-new/{{target}}
    cargo +{{fuzz_toolchain}} fuzz run --target "$(just _fuzz-host)" {{target}} target/fuzz-new/{{target}} fuzz/corpus/{{target}} -- -max_total_time={{secs}} -rss_limit_mb=4096 -timeout=30 -max_len=$(just _fuzz-max-len {{target}})

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
        cargo +{{fuzz_toolchain}} fuzz cmin --target "$(just _fuzz-host)" "$target" "$dir" -- -max_len="$(just _fuzz-max-len "$target")"
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
        wal_replay|otlp_http_protobuf|otlp_http_json|otlp_grpc|saml_response|saml_metadata|saml_assertion) echo 262144 ;;
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
    cargo test -p cetra-eval --test live -- --ignored --test-threads 1

# Supply-chain checks: advisories, bans, licences, sources (D-P-3), for the root graph and the fuzz graph (track R M3). The release workflow runs it before publishing (D196)
deny: fuzz-lock
    cargo deny check advisories bans licenses sources
    cargo deny --manifest-path fuzz/Cargo.toml check advisories bans licenses sources

# The checks that only change when the dependencies do: bans, licences, sources (D196). In `check-rust`; advisories run nightly
deny-policy: fuzz-lock
    cargo deny check bans licenses sources
    cargo deny --manifest-path fuzz/Cargo.toml check bans licenses sources

# Debug build
build:
    cargo build --workspace

# Build the web UI, then the release binary with the UI embedded (D-G-18)
build-release: js-install
    pnpm --dir web run build
    cargo build --release -p cetra-cli

# Build the release binary and check that the UI is embedded (D-G-37)
check-release: build-release
    ./target/release/cetra version | grep -qx 'ui: embedded'

# Local demo (docs/testing-locally.md): the release binary on target/demo, with a known admin,
# org `demo`, project `demo` and ingest key. The mock LLM on 127.0.0.1 needs private egress.
demo_key := "sk-cetra-demo0000000000000000000000000000"

# Start the release binary (`just build-release` first) on target/demo with a known admin
demo-serve:
    test -x target/release/cetra || { echo "run 'just build-release' first"; exit 1; }
    CETRA_DATA_DIR=target/demo/data \
    CETRA_INIT_ADMIN_EMAIL=admin@demo.test \
    CETRA_INIT_ADMIN_PASSWORD=demo-password \
    CETRA_INIT_ORG=demo \
    CETRA_INIT_PROJECT=demo \
    CETRA_INIT_SECRET_KEY={{demo_key}} \
    CETRA_SECURITY__ALLOW_PRIVATE_NETWORK_EGRESS=true \
    ./target/release/cetra serve

# Send demo traces and scores to the server started by `just demo-serve` (or the enterprise demo on 8081)
demo-seed traces="300" url="http://localhost:8080":
    python3 tools/demo/seed.py --url {{url}} --key {{demo_key}} --traces {{traces}} --email admin@demo.test --password demo-password

# OpenAI-compatible mock judge and webhook receiver on 127.0.0.1:8090
demo-llm:
    python3 tools/demo/mock_llm.py --port 8090

# The release binary, trusting the development licence key like a debug build (enterprise demo)
build-release-dev-license: js-install
    pnpm --dir web run build
    cargo build --release -p cetra-cli --config 'profile.release.package.cetra-token.debug-assertions=true'

# Print an enterprise licence for the demo, signed with the development key
demo-license:
    @cargo run -q -p cetra-license-tool -- sign --key fixtures/license/dev-1.key --kid dev-1 --payload tools/demo/enterprise/license.json

# Run the binary with arguments, e.g. `just run --version`
run *args:
    cargo run -p cetra-cli -- {{args}}

# Run the demo data generator (tools/demo), e.g. `just demo -- --help`
demo *args:
    cargo run -q -p cetra-demo -- {{args}}

# Build the cetra-demo image (D187): a static musl binary and the fixtures, staged in target/demo-image, then the image from that directory (tools/demo/Dockerfile)
demo-image tag="cetra-demo:dev":
    CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --locked -p cetra-demo --target x86_64-unknown-linux-musl
    rm -rf target/demo-image
    mkdir -p target/demo-image/fixtures
    cp "${CARGO_TARGET_DIR:-target}/x86_64-unknown-linux-musl/release/cetra-demo" target/demo-image/
    cp -r fixtures/otlp target/demo-image/fixtures/otlp
    docker build -f tools/demo/Dockerfile -t {{tag}} target/demo-image

# Rust checks: format, lint, tests, licence headers, golden directories, cargo deny's bans, licences and sources (D196)
check-rust: fmt-check lint test check-ee check-golden deny-policy

# Install JS dependencies from the lockfile
js-install:
    pnpm install --frozen-lockfile

# Web UI checks: lint, types, tests, production build
check-web: js-install
    pnpm --dir web run check

# The web checks without the tests (API types, Biome, types, production build); CI's `web` job
check-web-static: js-install
    pnpm --dir web run check:static

# One shard of the web tests in CI (`shard` of `total`, vitest --shard; .github/CI.md § Web tests)
test-web-shard shard total: js-install
    pnpm --dir web run test --shard={{shard}}/{{total}}

# Python SDK checks: format, lint, types, tests
check-py:
    cd sdks/python && uv sync --locked && uv run ruff format --check && uv run ruff check && uv run mypy && uv run pytest

# TypeScript SDK checks: lint, types, tests, package
check-ts: js-install
    pnpm --dir sdks/typescript run check

# The cetra-cli package: lint, types, build, tests, package lint (D-AO-2)
check-cli-js: js-install
    pnpm --dir node-cli run check

# Documentation site checks: lint, script tests, types, content checks, build with link validation (D-W-13)
check-docs: js-install
    pnpm --dir website run check

# Rewrite the docs' UI screenshots from the web UI on its mocks (D-W-12). Needs Chromium:
# `pnpm --dir website exec playwright install chromium` once. Optional: shot names.
docs-screenshots *names: js-install
    cd website && node scripts/screenshots.mjs {{names}}

# Every check (D79); must pass before every commit
check: check-rust check-web check-py check-ts check-cli-js check-docs

# Build the web, run the Rust e2e tests with the UI embedded, then the SDK suites against a real server (D-G-20, D-G-18)
e2e: js-install
    pnpm --dir web run build
    E2E_REQUIRE_UI=1 cargo test --workspace --all-features --test e2e
    bash tools/e2e/run.sh
    ./target/debug/cetra version | grep -qx 'ui: embedded'

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
    EXPECTED="$expected" node --input-type=module -e 'import { VERSION } from "@cetra-ai/sdk"; if (VERSION !== process.env.EXPECTED) { console.error(`ESM: VERSION ${VERSION}`); process.exit(1); }'
    EXPECTED="$expected" node -e 'const { VERSION } = require("@cetra-ai/sdk"); if (VERSION !== process.env.EXPECTED) { console.error(`CJS: VERSION ${VERSION}`); process.exit(1); }'
    echo "cetra ${expected} loads from ESM and CommonJS on Node $(node --version)"

# Install a packed cetra-cli into a scratch project and check that its bin prints the tarball's version (D-AO-2)
cli-smoke tarball:
    #!/usr/bin/env bash
    set -euo pipefail
    tarball="$(realpath '{{tarball}}')"
    expected="$(tar -xOzf "$tarball" package/package.json | node -p 'JSON.parse(require("fs").readFileSync(0, "utf8")).version')"
    dir="$(mktemp -d)"
    trap 'rm -rf "$dir"' EXIT
    cd "$dir"
    npm init -y >/dev/null
    # The same 7-day age gate as ts-smoke (D74); npm 10, shipped with Node 22.12, supports --before.
    before="$(node -p 'new Date(Date.now() - 7*24*3600*1000).toISOString()')"
    npm install --no-audit --no-fund --ignore-scripts --before "$before" "$tarball" >/dev/null
    actual="$(node_modules/.bin/cetra-cli --version)"
    if [ "$actual" != "$expected" ]; then
        echo "cetra-cli --version prints $actual, not $expected" >&2
        exit 1
    fi
    echo "cetra-cli ${expected} runs on Node $(node --version)"

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
    for dir in crates/server/src/ee web/src/ee tools/license crates/token crates/saml; do
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
            crates/server/src/ee/*|web/src/ee/*|tools/license/*|crates/token/*|crates/saml/*) ;;
            *) echo "$file: Elastic-2.0 header outside an ee directory"; status=1 ;;
        esac
    done < <(git grep -l -F 'SPDX-License-Identifier: Elastic-2.0' -- '*.rs' '*.ts' '*.tsx' || true)
    exit "$status"

# Re-sign fixtures/license/payloads with the dev key (D-P-6)
license-fixtures:
    cargo run -q -p cetra-license-tool -- fixtures

# Web dev server with mocked API (fixtures/api)
web-dev: js-install
    pnpm --dir web run dev

# Web dev server proxied at the real API (no mocks)
web-dev-server: js-install
    pnpm --dir web run dev:server

# Documentation site dev server on http://localhost:4321
docs-dev: js-install
    pnpm --dir website run dev

# Needs CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID (or a `wrangler login`); the
# last line printed is `url=<deployed URL>`.
# Build the site and deploy it to the `cetra-site` Worker: `main` is production, `pr-<number>` a Preview (D-W-3)
docs-deploy branch: js-install
    #!/usr/bin/env bash
    set -euo pipefail
    pnpm --dir website run build
    case "{{branch}}" in
      main) out="$(pnpm --dir website exec wrangler deploy)" ;;
      pr-[0-9]*) out="$(pnpm --dir website exec wrangler preview --name "{{branch}}")" ;;
      *) echo "branch must be main or pr-<number>, not {{branch}}" >&2; exit 1 ;;
    esac
    echo "$out"
    if [ "{{branch}}" = main ]; then
      url="$(grep -oE 'https://cetra-site\.[a-z0-9-]+\.workers\.dev' <<<"$out" | head -n1)"
    else
      url="$(grep -oE 'https://{{branch}}-cetra-site\.[a-z0-9-]+\.workers\.dev' <<<"$out" | head -n1)"
    fi
    test -n "$url" || { echo "no workers.dev URL in the wrangler output" >&2; exit 1; }
    echo "url=$url/"

# Needs CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID (or a `wrangler login`); the
# last line printed is `url=<deployed URL>`.
# Build the web UI on its mocks and deploy it to the `cetra-web-demo` Worker: `main` is production, `pr-<number>` a Preview (D-C-22)
web-demo-deploy branch: js-install
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{branch}}" in
      main | pr-[0-9]*) ;;
      *) echo "branch must be main or pr-<number>, not {{branch}}" >&2; exit 1 ;;
    esac
    pnpm --dir web run build:demo
    test -f web/dist-demo/mockServiceWorker.js || { echo "the demo build has no mockServiceWorker.js" >&2; exit 1; }
    if [ "{{branch}}" = main ]; then
      out="$(pnpm --dir web exec wrangler deploy)"
    else
      out="$(pnpm --dir web exec wrangler preview --name "{{branch}}")"
    fi
    echo "$out"
    if [ "{{branch}}" = main ]; then
      url="$(grep -oE 'https://cetra-web-demo\.[a-z0-9-]+\.workers\.dev' <<<"$out" | head -n1)"
    else
      url="$(grep -oE 'https://{{branch}}-cetra-web-demo\.[a-z0-9-]+\.workers\.dev' <<<"$out" | head -n1)"
    fi
    test -n "$url" || { echo "no workers.dev URL in the wrangler output" >&2; exit 1; }
    echo "url=$url/"

# Check the docs site's external links (D-W-13); needs lychee 0.24.2 and builds the site first.
# Root-relative links resolve in website/dist. The https://cetra.sh/ URLs (canonical links) are
# skipped: starlight-links-validator checks the site's own pages during the build.
docs-links: js-install
    pnpm --dir website run build
    lychee --no-progress --max-concurrency 8 --accept 200,206,429 --root-dir "$PWD/website/dist" --exclude '^http://localhost' --exclude '^https://cetra\.sh/' --exclude-path website/dist/docs/reference/api 'website/dist/**/*.html'

# Regenerate crates/server/openapi.json and web/src/api/schema.gen.ts
openapi: js-install
    CETRA_OPENAPI_WRITE=1 cargo test -p cetra-server --test openapi -- contract_file_is_up_to_date
    pnpm --dir web run gen:api

# Regenerate the generated docs pages (D-W-7): the CLI and SQL schema references, and the skill references (D-AO-8)
docs-gen: js-install
    CETRA_DOCS_WRITE=1 cargo test -p cetra-cli cli_reference_is_up_to_date
    CETRA_DOCS_WRITE=1 cargo test -p cetra-server sql_schema_reference_is_up_to_date
    cd website && CETRA_DOCS_WRITE=1 node scripts/skill-references.mjs

# Copy the agent skill and its plugin manifests into a checkout of cetra-ai/skills (D-AO-9); --push commits and pushes
skills-mirror dest *flags:
    tools/skills/mirror.sh "{{dest}}" {{flags}}

# Regenerate fixtures/api responses and manifest
api-examples:
    CETRA_API_EXAMPLES_WRITE=1 cargo test -p cetra-server --lib api_examples::examples_are_up_to_date

# Validate OTLP fixtures
fixtures-check:
    cargo test -p cetra-otel --test fixtures

# Rewrite OTLP fixtures in canonical JSON formatting
fixtures-fmt:
    CETRA_FIXTURES_FMT=1 cargo test -p cetra-otel --test fixtures -- canonical_formatting

# Rewrite expected.json from the normalizer. Review the diff before committing.
fixtures-bless:
    CETRA_BLESS=1 cargo test -p cetra-otel --test golden

# Regenerate the built-in price snapshot from a downloaded LiteLLM JSON (D104).
# Download model_prices_and_context_window.json from
# https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json
# by hand. This recipe does not fetch it.
prices-update litellm_json:
    CETRA_LITELLM_JSON={{litellm_json}} cargo test -p cetra-otel --test prices -- refresh

# Fixture capture

# Capture OTLP fixtures from real instrumentations (tools/capture), e.g. `just capture openinference/openai-chat-tool`
capture +cases: capture-py-sync capture-ts-sync
    cargo run -q -p cetra-capture -- run {{cases}}

# Recapture every scenario
capture-all: capture-py-sync capture-ts-sync
    cargo run -q -p cetra-capture -- run --all

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
    cargo run -q -p cetra-capture -- run --all --check

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
image tag="cetra:dev":
    docker build -f deploy/docker/Dockerfile -t {{tag}} .

# Run an image with a fresh volume and smoke-test it, healthcheck included (D-N-13)
image-smoke tag="cetra:dev":
    tools/packaging/image-smoke.sh {{tag}}

# Run deploy/install.sh against target/dist served on loopback (D-N-15)
install-smoke:
    tools/packaging/install-smoke.sh

# Run deploy/compose with an image and smoke-test it; `just compose-smoke cetra:dev s3` adds Garage (D-N-9)
compose-smoke tag="cetra:dev" *mode:
    tools/packaging/compose-smoke.sh {{tag}} {{mode}}

# Lint the Helm chart and validate its manifests with kubeconform (D-N-10)
helm-check:
    tools/packaging/helm-check.sh

# Install the chart with an image on a throwaway kind cluster and smoke-test it (D-N-10)
helm-smoke tag="cetra:dev":
    tools/packaging/helm-smoke.sh {{tag}}

# Check that the chart and compose files carry the workspace version (D-N-5)
versions-check:
    tools/packaging/versions-check.sh

# Set the release version everywhere and date the changelog section, e.g. `just release-prep 0.1.0`
# (D-N-5); a stable version also writes its golden data directory (D-R-30)
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
    just binary-smoke target/x86_64-unknown-linux-musl/release/cetra
    just install-smoke
    just image cetra:dev
    just image-smoke cetra:dev
    just compose-smoke cetra:dev
    just compose-smoke cetra:dev s3
