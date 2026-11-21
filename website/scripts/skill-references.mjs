// Generates skills/cetra/references/ from the docs pages that skills/references.json lists (D-AO-8).
// Checks them by default; CETRA_DOCS_WRITE=1 rewrites them (`just docs-gen`).
// Prints one finding per line to stderr and exits 1 when there is any.
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { toMarkdown } from "./markdown.mjs";

/** Paths relative to website/ (the cwd of `pnpm check`). */
export const LIST = "../skills/references.json";
export const PAGES = "src/content/docs/docs";
export const OUT = "../skills/cetra/references";

const NAME = /^[a-z0-9]+(-[a-z0-9]+)*$/;

/** The header line of every generated file. */
export function header(page) {
  return `<!-- Generated from website/src/content/docs/docs/${page} by website/scripts/skill-references.mjs. Do not edit: run \`just docs-gen\`. -->\n\n`;
}

/**
 * Validates the parsed references.json; returns error messages (empty when valid).
 * Each entry needs `name` matching /^[a-z0-9]+(-[a-z0-9]+)*$/ and a string `page`
 * ending in `.md` or `.mdx`; names are unique; `pageExists(page)` must be true.
 */
export function validateList(list, pageExists) {
  const errors = [];
  const names = new Set();
  list.forEach((entry, i) => {
    const where = `references[${i}]`;
    const name = entry?.name;
    if (typeof name !== "string" || !NAME.test(name)) {
      errors.push(`${where}: name ${JSON.stringify(name)} must match ${NAME}`);
    } else if (names.has(name)) {
      errors.push(`${where}: duplicate name ${name}`);
    } else {
      names.add(name);
    }
    const page = entry?.page;
    if (typeof page !== "string" || !/\.mdx?$/.test(page)) {
      errors.push(`${where}: page ${JSON.stringify(page)} must end in .md or .mdx`);
    } else if (!pageExists(page)) {
      errors.push(`${where}: page ${page} does not exist`);
    }
  });
  return errors;
}

/**
 * The expected files: Map of `<name>.md` to header(page) + toMarkdown(readPage(page)).
 * A converter error is thrown as `<page>: <message>`.
 */
export function expectedFiles(list, readPage) {
  const files = new Map();
  for (const { name, page } of list) {
    try {
      files.set(`${name}.md`, header(page) + toMarkdown(readPage(page)));
    } catch (error) {
      throw new Error(`${page}: ${error.message}`);
    }
  }
  return files;
}

/**
 * Compares expected and actual (Map of file name to text) and returns messages:
 * stale, missing, or not in skills/references.json. Sorted by file name.
 */
export function diffFiles(expected, actual) {
  const messages = [];
  const names = [...new Set([...expected.keys(), ...actual.keys()])].sort();
  for (const file of names) {
    const where = `skills/cetra/references/${file}`;
    if (!expected.has(file)) {
      messages.push(`${where} is not in skills/references.json: run \`just docs-gen\``);
    } else if (!actual.has(file)) {
      messages.push(`${where} is missing: run \`just docs-gen\``);
    } else if (expected.get(file) !== actual.get(file)) {
      messages.push(`${where} is stale: run \`just docs-gen\``);
    }
  }
  return messages;
}

/** The `.md` files in OUT, as a Map of file name to text; empty when OUT is missing. */
function readReferences() {
  const actual = new Map();
  if (!existsSync(OUT)) return actual;
  for (const file of readdirSync(OUT)) {
    if (file.endsWith(".md")) actual.set(file, readFileSync(join(OUT, file), "utf8"));
  }
  return actual;
}

export function main(write) {
  let list;
  try {
    list = JSON.parse(readFileSync(LIST, "utf8"));
  } catch (error) {
    console.error(`skills/references.json: ${error.message}`);
    return 1;
  }
  if (!Array.isArray(list?.references)) {
    console.error("skills/references.json: references must be an array");
    return 1;
  }
  const errors = validateList(list.references, (page) => existsSync(join(PAGES, page)));
  for (const error of errors) console.error(`skills/references.json: ${error}`);
  if (errors.length > 0) return 1;

  let expected;
  try {
    expected = expectedFiles(list.references, (page) => readFileSync(join(PAGES, page), "utf8"));
  } catch (error) {
    console.error(error.message);
    return 1;
  }

  if (write) {
    mkdirSync(OUT, { recursive: true });
    for (const file of readReferences().keys()) {
      if (!expected.has(file)) rmSync(join(OUT, file));
    }
    for (const [file, text] of expected) writeFileSync(join(OUT, file), text);
    console.error(`wrote ${expected.size} references`);
    return 0;
  }

  const messages = diffFiles(expected, readReferences());
  for (const message of messages) console.error(message);
  return messages.length === 0 ? 0 : 1;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  process.exit(main(Boolean(process.env.CETRA_DOCS_WRITE)));
}
