#!/usr/bin/env bash
# Writes target/dist/THIRD-PARTY-NOTICES.md: the licences of the Rust crates and of the web
# packages compiled into the `llmobs` binary (D-N-8). Needs cargo-about, node and pnpm, and
# `pnpm install` done (just js-install).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
mkdir -p target/dist
rust="$(mktemp)"
js="$(mktemp)"
trap 'rm -f "$rust" "$js"' EXIT

cargo about generate --locked --fail \
  -c tools/packaging/about.toml \
  -m crates/cli/Cargo.toml \
  -o "$rust" \
  tools/packaging/about.hbs
pnpm --dir web licenses list --prod --json | node tools/packaging/js-notices.mjs >"$js"

{
  echo "# Third-party notices"
  echo
  echo "The \`llmobs\` binary contains third-party software. This file lists it with its licences:"
  echo "first the Rust crates, then the JavaScript packages of the embedded web UI."
  echo
  cat "$rust"
  echo
  cat "$js"
} >target/dist/THIRD-PARTY-NOTICES.md
echo "wrote target/dist/THIRD-PARTY-NOTICES.md"
