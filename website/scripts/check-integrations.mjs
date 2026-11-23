// Integrations check (IN2.8): every integration page has exactly one row in the index, and the
// row's "Tested against" tier is true of the page's <Backing> box and of the fixtures' `origin`.
// Prints `path:line: message` per finding and exits 1 when there is any.
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const TIER = /^(captured|hand-written|SDK test|setup only)( \((Python|TypeScript)\))?$/;
const ROW = /^\| \[([^\]]+)\]\(\/docs\/integrations\/([a-z0-9-]+)\/\) \|(.*)\|\s*$/;

/** Reads a `<Backing>` box's `fixtures` and `tests` lists. */
function boxes(text) {
  return [...text.matchAll(/<Backing\b[\s\S]*?\/>/g)].map((box) => {
    const list = (name) => {
      const m = box[0].match(new RegExp(`${name}=\\{\\[([\\s\\S]*?)\\]\\}`));
      return m ? [...m[1].matchAll(/"([^"]*)"/g)].map((x) => x[1]) : [];
    };
    return { fixtures: list("fixtures"), tests: list("tests") };
  });
}

/**
 * `pages` maps a slug to its `.mdx` text; `caseOrigin(group/case)` returns the `origin` of that
 * case's `case.json`, or null when there is none.
 */
export function checkIndex(indexText, pages, caseOrigin) {
  const findings = [];
  const rows = new Map();
  indexText.split("\n").forEach((line, i) => {
    const m = line.match(ROW);
    if (!m) return;
    const slug = m[2];
    const lineNo = i + 1;
    rows.set(slug, (rows.get(slug) ?? 0) + 1);
    const tierText = m[3]
      .split("|")
      .map((c) => c.trim())
      .pop();
    const page = pages[slug];
    if (page === undefined) {
      findings.push(`index.mdx:${lineNo}: no page for ${slug}`);
      return;
    }
    const pageBoxes = boxes(page);
    const fixtures = pageBoxes.flatMap((b) => b.fixtures);
    const tests = pageBoxes.flatMap((b) => b.tests);
    for (const part of tierText.split(", ")) {
      const t = part.match(TIER);
      if (!t) {
        findings.push(`index.mdx:${lineNo}: unknown tier "${part}" for ${slug}`);
        continue;
      }
      const tier = t[1];
      const origins = fixtures.map((f) => caseOrigin(f));
      let ok;
      if (tier === "captured") ok = origins.includes("captured");
      else if (tier === "hand-written") ok = origins.includes("hand_written");
      else if (tier === "SDK test") ok = tests.length > 0;
      else ok = pageBoxes.length === 0;
      if (!ok) {
        findings.push(`index.mdx:${lineNo}: ${slug} is "${tier}" but its page does not show that`);
      }
    }
  });
  for (const slug of Object.keys(pages)) {
    const n = rows.get(slug) ?? 0;
    if (n !== 1) findings.push(`${slug}.mdx:1: has ${n} rows in the index, expected 1`);
  }
  return findings;
}

export function main(root, fixturesRoot) {
  const caseOrigin = (name) => {
    const path = join(fixturesRoot, name, "case.json");
    if (!existsSync(path)) return null;
    return JSON.parse(readFileSync(path, "utf8")).origin ?? null;
  };
  const pages = {};
  for (const entry of readdirSync(root)) {
    if (!/\.mdx$/.test(entry) || entry === "index.mdx") continue;
    pages[entry.replace(/\.mdx$/, "")] = readFileSync(join(root, entry), "utf8");
  }
  const findings = checkIndex(readFileSync(join(root, "index.mdx"), "utf8"), pages, caseOrigin);
  for (const f of findings) console.error(f);
  return findings.length === 0 ? 0 : 1;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  process.exit(main("src/content/docs/docs/integrations", "../fixtures/otlp"));
}
