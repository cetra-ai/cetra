# N2 — Third-party licence notices

Read [README.md](README.md) first, especially § Design › Licence notices.

**Goal:** `just notices` writes `target/dist/THIRD-PARTY-NOTICES.md` (D-N-8). The file lists the
licences of everything compiled into the `cetra` binary:

- the Rust crates of `cetra-cli` for the three release targets, from `cargo about`;
- the JavaScript packages of the web UI, from `pnpm licenses list` and a small Node script that
  copies each package's licence file.

N3 puts the file in every archive and in the image. The file is generated, never committed.

**Edit only:**
- `tools/packaging/about.toml`, `tools/packaging/about.hbs`, `tools/packaging/js-notices.mjs`,
  `tools/packaging/notices.sh` (all new);
- `justfile`;
- `biome.json`.

**Commit:** `Add third-party licence notices`

**Tools:** cargo-about 0.9.3 (`cargo about --version`), Node on `PATH` (README § Local tools).

## Files

### `tools/packaging/about.toml` (new)

`accepted` is the allow-list of `deny.toml`'s `[licenses]` as P1 writes it. Workspace crates are
private and are covered by `LICENSE`. Build and dev dependencies are not in the binary.

```toml
# Third-party notices for the cetra binary (D-N-8). Same allow-list as deny.toml [licenses] as
# track P's P1 writes it (docs/plans/p-entitlements/01-m1-open-core-license.md § P1).
accepted = [
    "0BSD",
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "BSD-1-Clause",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "BSL-1.0",
    "CC0-1.0",
    "CDLA-Permissive-2.0",
    "ISC",
    "MIT",
    "MIT-0",
    "MPL-2.0",
    "Unicode-3.0",
    "Unlicense",
    "Zlib",
]
targets = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "aarch64-apple-darwin",
]
ignore-build-dependencies = true
ignore-dev-dependencies = true

[private]
ignore = true
```

### `tools/packaging/about.hbs` (new)

A Handlebars template. `{{{text}}}` has three braces so the licence text is not HTML-escaped.

````handlebars
# Third-party notices: Rust crates

The `cetra` binary includes the following Rust crates. Each licence text is
followed by the crates it covers.

{{#each licenses}}
## {{name}} ({{id}})

Used by:

{{#each used_by}}
- {{crate.name}} {{crate.version}}{{#if crate.repository}} ({{crate.repository}}){{/if}}
{{/each}}

```text
{{{text}}}
```

{{/each}}
````

### `tools/packaging/js-notices.mjs` (new)

````js
// Reads `pnpm licenses list --prod --json` on stdin and prints a Markdown section with each
// package of the web UI and its licence text (D-N-8). Used by tools/packaging/notices.sh.
//
// pnpm prints an object keyed by licence id; each value is a list of packages with `name`,
// `versions` (list) and `paths` (list, the installed package directories).
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const LICENSE_FILE = /^(licen[cs]e|copying|notice)([-._][^/]*)?$/i;

function licenseTexts(dir) {
  let names;
  try {
    names = readdirSync(dir);
  } catch {
    return [];
  }
  return names
    .filter((name) => LICENSE_FILE.test(name))
    .sort()
    .map((name) => readFileSync(join(dir, name), "utf8").trim());
}

const input = JSON.parse(readFileSync(0, "utf8"));
const packages = [];
for (const [license, list] of Object.entries(input)) {
  for (const pkg of list) {
    packages.push({
      name: pkg.name,
      versions: pkg.versions ?? [pkg.version],
      paths: pkg.paths ?? [pkg.path],
      license,
    });
  }
}
packages.sort((a, b) => a.name.localeCompare(b.name));

const out = [];
out.push("# Third-party notices: web UI packages", "");
out.push("The web UI embedded in the `cetra` binary includes the following packages.", "");
let missing = 0;
for (const pkg of packages) {
  out.push(`## ${pkg.name} ${pkg.versions.join(", ")} (${pkg.license})`, "");
  const texts = pkg.paths.flatMap(licenseTexts);
  if (texts.length === 0) {
    missing += 1;
    out.push(
      `No licence file is shipped in the package; its declared licence is ${pkg.license}.`,
      "",
    );
    continue;
  }
  // Every version of a package usually ships the same file; print each distinct text once.
  for (const text of [...new Set(texts)]) {
    out.push("```text", text, "```", "");
  }
}
process.stdout.write(`${out.join("\n")}\n`);
process.stderr.write(
  `js-notices: ${packages.length} packages, ${missing} without a licence file\n`,
);
````

### `tools/packaging/notices.sh` (new, executable)

```bash
#!/usr/bin/env bash
# Writes target/dist/THIRD-PARTY-NOTICES.md: the licences of the Rust crates and of the web
# packages compiled into the `cetra` binary (D-N-8). Needs cargo-about, node and pnpm, and
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
  echo "The \`cetra\` binary contains third-party software. This file lists it with its licences:"
  echo "first the Rust crates, then the JavaScript packages of the embedded web UI."
  echo
  cat "$rust"
  echo
  cat "$js"
} >target/dist/THIRD-PARTY-NOTICES.md
echo "wrote target/dist/THIRD-PARTY-NOTICES.md"
```

Run `chmod +x tools/packaging/notices.sh`.

### `justfile`

At the end of the file, add an empty line, then exactly:

```just
# Packaging and release (track N, docs/plans/n-packaging/)

# Write target/dist/THIRD-PARTY-NOTICES.md: licences of the crates and web packages in the binary (D-N-8)
notices: js-install
    tools/packaging/notices.sh
```

Later sub-phases add their recipes after this one.

### `biome.json`

In `files.includes`, add `"tools/packaging/**"` after `"tools/capture/ts/**"`. Then `just fmt`
formats the script, and `pnpm exec biome check tools/packaging` lints it.

## Verify

```bash
export PATH="$HOME/.local/share/nvm/v26.13.0/bin:$PATH"
just notices
grep -c '^## ' target/dist/THIRD-PARTY-NOTICES.md
grep -q '^## .*(MIT)' target/dist/THIRD-PARTY-NOTICES.md
grep -q '^## react ' target/dist/THIRD-PARTY-NOTICES.md
grep -q '^- tokio 1\.' target/dist/THIRD-PARTY-NOTICES.md
pnpm exec biome check tools/packaging
just check
just assert-clean
```

- `just notices` prints `js-notices: <n> packages, <m> without a licence file` on stderr, then
  `wrote target/dist/THIRD-PARTY-NOTICES.md`. When this plan was written, n was 356 and m was 1
  (`react-remove-scroll-bar`).
- The file is about 1.2 MB, with about 540 `## ` lines: one per Rust licence text and one per web
  package, plus a few headings inside licence texts themselves.
- `cargo about generate --fail` stops on a crate whose licence it cannot identify, or whose
  licence is not in `accepted`. If that happens, stop and report the crate and its licence. Do not
  add a licence or a `[[<crate>.clarify]]` entry yourself.

## Done when

- [ ] The four files exist; `notices.sh` is mode `100755` in `git ls-files -s`.
- [ ] `just notices` writes the file and the greps above pass.
- [ ] `just check` and `just assert-clean` pass. One commit: `Add third-party licence notices`.
