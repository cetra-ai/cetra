import { readFileSync } from "node:fs";

/** The `## ` group of the migration guides. The homepage leaves them out. */
export const MIGRATING_GROUP = "Migrating";

/**
 * @typedef {object} Test
 * @property {"recorded" | "sdk" | "handwritten"} kind "recorded": spans captured from the real library; "sdk": an SDK test; "handwritten": spans written by hand
 * @property {"Python" | "TypeScript" | null} lang the language the test covers, or null for both
 */

/**
 * @typedef {object} Integration
 * @property {string} group     the `## ` heading it sits under, e.g. "Providers"
 * @property {string} title     the LinkCard title
 * @property {string} description the LinkCard description
 * @property {string} href      the docs route, e.g. "/docs/integrations/openai/"
 * @property {boolean} python   ✓ in the Python column
 * @property {boolean} typescript ✓ in the TypeScript column
 * @property {Test[]} tests    the "Tested against" cell, parsed
 */

const ROW =
  /^\| \[([^\]]+)\]\((\/docs\/integrations\/[a-z0-9-]+\/)\) \| (✓|—) \| (✓|—) \| (.*?) \| (.*?) \|\s*$/;
const CARD = /<LinkCard title="([^"]+)" description="([^"]+)" href="([^"]+)" \/>\s*$/;
const TEST = /^(captured|SDK test|hand-written)(?: \((Python|TypeScript)\))?$/;

/** The "Tested against" wording in the docs table, mapped to `Test.kind`. */
const KIND = {
  captured: "recorded",
  "SDK test": "sdk",
  "hand-written": "handwritten",
};

/**
 * @param {string} tested the "Tested against" cell without backticks, e.g. "captured (Python), SDK test (TypeScript)"
 * @param {string} where  for the error message
 * @returns {Test[]}
 */
function parseTested(tested, where) {
  return tested.split(", ").map((part) => {
    const match = TEST.exec(part);
    if (!match) throw new Error(`integrations: unknown test wording "${part}" at ${where}`);
    return { kind: KIND[match[1]], lang: match[2] ?? null };
  });
}

/**
 * @param {string} mdx
 * @returns {Integration[]}
 */
export function parseIntegrations(mdx) {
  /** @type {Map<string, {python: boolean, typescript: boolean, tests: Test[]}>} */
  const rows = new Map();
  /** @type {{group: string, title: string, description: string, href: string}[]} */
  const cards = [];
  let group = "";

  mdx.split(/\r?\n/).forEach((line, index) => {
    const where = `line ${index + 1}`;
    if (line.startsWith("| [")) {
      const match = ROW.exec(line);
      if (!match) {
        throw new Error(`integrations: malformed table row at ${where}`);
      }
      const [, , href, python, typescript, , tested] = match;
      if (rows.has(href)) {
        throw new Error(`integrations: duplicate table row ${href} at ${where}`);
      }
      rows.set(href, {
        python: python === "✓",
        typescript: typescript === "✓",
        tests: parseTested(tested.replaceAll("`", ""), where),
      });
    } else if (line.startsWith("## ")) {
      group = line.slice(3).trim();
    } else {
      const match = CARD.exec(line);
      if (match) {
        const [, title, description, href] = match;
        if (cards.some((card) => card.href === href)) {
          throw new Error(`integrations: duplicate card ${href} at ${where}`);
        }
        cards.push({ group, title, description, href });
      }
    }
  });

  const cardHrefs = new Set(cards.map((card) => card.href));
  for (const href of rows.keys()) {
    if (!cardHrefs.has(href)) {
      throw new Error(`integrations: no card for table row ${href}`);
    }
  }

  return cards.map((card) => {
    const row = rows.get(card.href);
    if (!row) {
      throw new Error(`integrations: no table row for card ${card.href}`);
    }
    return { ...card, ...row };
  });
}

/**
 * Reads the docs page from disk. The cwd must be `website/`: `astro build`, `astro check` and
 * `pnpm --dir website exec node --test` all run there; `node --test` from the repo root does not.
 * @returns {Integration[]}
 */
export function loadIntegrations(path = "src/content/docs/docs/integrations/index.mdx") {
  return parseIntegrations(readFileSync(path, "utf8"));
}

/**
 * The groups in page order.
 * @param {Integration[]} items
 */
export function groups(items) {
  return [...new Set(items.map((item) => item.group))];
}
