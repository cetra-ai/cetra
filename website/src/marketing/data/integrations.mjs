import { readFileSync } from "node:fs";

/**
 * @typedef {object} Integration
 * @property {string} group     the `## ` heading it sits under, e.g. "Providers"
 * @property {string} title     the LinkCard title
 * @property {string} description the LinkCard description
 * @property {string} href      the docs route, e.g. "/docs/integrations/openai/"
 * @property {boolean} python   ✓ in the Python column
 * @property {boolean} typescript ✓ in the TypeScript column
 * @property {string} tested    the "Tested against" cell, e.g. "captured"
 */

const ROW =
  /^\| \[([^\]]+)\]\((\/docs\/integrations\/[a-z0-9-]+\/)\) \| (✓|—) \| (✓|—) \| (.*?) \| (.*?) \|$/;
const CARD = /<LinkCard title="([^"]+)" description="([^"]+)" href="([^"]+)" \/>/;

/**
 * @param {string} mdx
 * @returns {Integration[]}
 */
export function parseIntegrations(mdx) {
  /** @type {Map<string, {python: boolean, typescript: boolean, tested: string}>} */
  const rows = new Map();
  /** @type {{group: string, title: string, description: string, href: string}[]} */
  const cards = [];
  let group = "";

  mdx.split("\n").forEach((line, index) => {
    if (line.startsWith("| [")) {
      const match = ROW.exec(line);
      if (!match) {
        throw new Error(`integrations: malformed table row at line ${index + 1}`);
      }
      const [, , href, python, typescript, , tested] = match;
      rows.set(href, {
        python: python === "✓",
        typescript: typescript === "✓",
        tested: tested.replaceAll("`", ""),
      });
    } else if (line.startsWith("## ")) {
      group = line.slice(3).trim();
    } else {
      const match = CARD.exec(line);
      if (match) {
        const [, title, description, href] = match;
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
