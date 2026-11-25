// Backing check for the integration pages (W8): every `fixtures={[…]}` value of a `<Backing>` box
// names an existing directory `fixtures/otlp/<group>/<case>`, and a box has fixtures or tests.
// Prints `path:line: message` per finding and exits 1 when there is any.
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

/** `exists(relative)` tells whether `fixtures/otlp/<relative>` is a directory. */
export function checkFile(path, text, exists) {
  const findings = [];
  for (const box of text.matchAll(/<Backing\b[\s\S]*?\/>/g)) {
    const line = text.slice(0, box.index).split("\n").length;
    const list = (name) => {
      const m = box[0].match(new RegExp(`${name}=\\{\\[([\\s\\S]*?)\\]\\}`));
      return m ? [...m[1].matchAll(/"([^"]*)"/g)].map((x) => x[1]) : [];
    };
    const fixtures = list("fixtures");
    const tests = list("tests");
    if (fixtures.length === 0 && tests.length === 0) {
      findings.push(`${path}:${line}: Backing box without fixtures or tests`);
    }
    for (const fixture of fixtures) {
      if (!/^[a-z0-9_]+\/[a-z0-9-]+$/.test(fixture) || !exists(fixture)) {
        findings.push(`${path}:${line}: no fixture case fixtures/otlp/${fixture}`);
      }
    }
  }
  return findings;
}

export function main(root, fixturesRoot) {
  const exists = (relative) => existsSync(join(fixturesRoot, relative, "case.json"));
  const findings = [];
  for (const entry of readdirSync(root, { recursive: true })) {
    if (!/\.mdx$/.test(entry)) continue;
    const path = join(root, entry);
    findings.push(...checkFile(path, readFileSync(path, "utf8"), exists));
  }
  for (const f of findings) console.error(f);
  return findings.length === 0 ? 0 : 1;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  process.exit(main("src/content/docs", "../fixtures/otlp"));
}
