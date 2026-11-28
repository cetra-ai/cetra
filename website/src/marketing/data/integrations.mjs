import { readFileSync } from "node:fs";

/** The `## ` group of the migration guides. The homepage leaves them out. */
export const MIGRATING_GROUP = "Migrating";

/**
 * @typedef {object} Test
 * @property {"recorded" | "sdk" | "handwritten" | "setup"} kind "recorded": spans captured from the real library; "sdk": an SDK test; "handwritten": spans written by hand; "setup": no spans or test in the repository
 * @property {"Python" | "TypeScript" | null} lang the language the test covers, or null for both
 */

/**
 * @typedef {Test & { kind: "recorded" | "sdk" | "handwritten" }} Tested
 */

/** @typedef {"python" | "typescript" | "java" | "kotlin" | "dotnet"} Language */

/** The chip text of each language, in display order. */
export const LANGUAGE_CHIPS = {
  python: "python",
  typescript: "typescript",
  java: "java",
  kotlin: "kotlin",
  dotnet: ".net",
};

/** The "How" cell prefix that names a language the table has no column for. */
const HOW_LANGUAGES = [
  ["Java, ", "java"],
  ["Kotlin, ", "kotlin"],
  [".NET, ", "dotnet"],
];

/** A "How" cell that opens with `Word, ` names a language `HOW_LANGUAGES` may not list. */
const LEADING_LANGUAGE = /^([A-Z.][A-Za-z0-9.#+]*), /;

/**
 * @typedef {object} Integration
 * @property {string} group     the `## ` heading it sits under, e.g. "Providers"
 * @property {string} title     the LinkCard title
 * @property {string} description the LinkCard description
 * @property {string} href      the docs route, e.g. "/docs/integrations/openai/"
 * @property {Language[]} languages ✓ columns and the "How" cell's leading language, in `LANGUAGE_CHIPS` order; empty for a tool used from any language (a coding agent, a gateway)
 * @property {Tested[]} tests  the "Tested against" cell, parsed; a card never has a "setup" test
 */

const ROW =
  /^\| \[([^\]]+)\]\((\/docs\/integrations\/[a-z0-9-]+\/)\) \| (✓|—) \| (✓|—) \| (.*?) \| (.*?) \|\s*$/;
const CARD = /<LinkCard title="([^"]+)" description="([^"]+)" href="([^"]+)" \/>\s*$/;
const TEST = /^(captured|SDK test|hand-written|setup only)(?: \((Python|TypeScript)\))?$/;

/** The "Tested against" wording in the docs table, mapped to `Test.kind`. */
const KIND = {
  captured: "recorded",
  "SDK test": "sdk",
  "hand-written": "handwritten",
  "setup only": "setup",
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
 * @param {boolean} python
 * @param {boolean} typescript
 * @param {string} how the "How" cell
 * @returns {Language[]}
 */
function languagesOf(python, typescript, how) {
  /** @type {Language[]} */
  const languages = [];
  if (python) languages.push("python");
  if (typescript) languages.push("typescript");
  for (const [prefix, language] of HOW_LANGUAGES) {
    if (how.startsWith(prefix)) languages.push(language);
  }
  return languages;
}

/**
 * @param {string} mdx
 * @returns {Integration[]}
 */
export function parseIntegrations(mdx) {
  /** @type {Map<string, {languages: Language[], unknownLanguage: string | null, tests: Test[]}>} */
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
      const [, , href, python, typescript, how, tested] = match;
      if (rows.has(href)) {
        throw new Error(`integrations: duplicate table row ${href} at ${where}`);
      }
      const languages = languagesOf(python === "✓", typescript === "✓", how);
      const leading = languages.length === 0 ? LEADING_LANGUAGE.exec(how) : null;
      rows.set(href, {
        languages,
        unknownLanguage: leading ? leading[1] : null,
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
  for (const [href, row] of rows) {
    // A setup-only row has no card and is left out of the list.
    const setupOnly = row.tests.every((test) => test.kind === "setup");
    if (!cardHrefs.has(href) && !setupOnly) {
      throw new Error(`integrations: no card for table row ${href}`);
    }
  }

  return cards.map((card) => {
    const row = rows.get(card.href);
    if (!row) {
      throw new Error(`integrations: no table row for card ${card.href}`);
    }
    const { unknownLanguage, ...rest } = row;
    if (row.tests.some((test) => test.kind === "setup")) {
      throw new Error(`integrations: card for setup-only row ${card.href}`);
    }
    if (unknownLanguage !== null) {
      throw new Error(
        `integrations: unknown language "${unknownLanguage}" in the How cell of ${card.href}; add it to HOW_LANGUAGES`,
      );
    }
    return { ...card, ...rest, tests: /** @type {Tested[]} */ (rest.tests) };
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

/**
 * The tile's lettered square: the first ASCII letter or digit of the title, upper-cased.
 * @param {string} title
 */
export function monogram(title) {
  const match = /[A-Za-z0-9]/.exec(title);
  return match ? match[0].toUpperCase() : "?";
}
