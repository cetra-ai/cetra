// Builds the docs search index after `astro build` and `md-pages.mjs` (D-AO-13): reads every
// `dist/docs/**/index.md` and writes `dist/mcp-index.json`. The Worker never imports this file.
import { existsSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import MiniSearch from "minisearch";
import { INDEX_OPTIONS } from "./docs-search.mjs";

export const DOCS = "dist/docs";
export const INDEX = "dist/mcp-index.json";

/** The heading-like lines are "##" to "####"; a line whose trimmed text starts with ``` opens or closes a fence. */
function isFence(line) {
  return line.trim().startsWith("```");
}

/** One document per Markdown page: url "/docs/<p>/", title (the "# " line), description (the first
 *  paragraph after it), headings ("##"–"####" lines outside fenced code, joined by "\n"), body (the rest:
 *  every line except the title line and the description paragraph). Throws when the page has no "# " line. */
export function documentFor(url, markdown) {
  const lines = markdown.split("\n");
  // prose[i] is true for a line outside fenced code; the fence lines themselves are not prose.
  const prose = [];
  let inFence = false;
  for (const line of lines) {
    if (isFence(line)) {
      inFence = !inFence;
      prose.push(false);
    } else {
      prose.push(!inFence);
    }
  }
  const titleAt = lines.findIndex((line, i) => prose[i] && /^# .+/.test(line));
  if (titleAt < 0) throw new Error(`${url}: no "# " title line`);
  const title = lines[titleAt].slice(2).trim();

  const used = new Set([titleAt]);
  const paragraph = [];
  let start = titleAt + 1;
  while (start < lines.length && lines[start].trim() === "") start += 1;
  for (let i = start; i < lines.length && prose[i] && lines[i].trim() !== ""; i += 1) {
    paragraph.push(lines[i].trim());
    used.add(i);
  }
  const description = paragraph.join(" ");

  const headings = [];
  for (let i = titleAt + 1; i < lines.length; i += 1) {
    const match = prose[i] ? /^#{2,4} (.+)$/.exec(lines[i]) : null;
    if (match) headings.push(match[1].trim());
  }

  const body = lines
    .filter((_, i) => !used.has(i))
    .join("\n")
    .trim();
  return { url, title, description, headings: headings.join("\n"), body };
}

/** Reads every built docs page and writes the index; returns 0 on success, 1 otherwise. */
export function main() {
  if (!existsSync(DOCS)) {
    console.error(`${DOCS} is missing: run astro build first`);
    return 1;
  }
  const pages = readdirSync(DOCS, { recursive: true }).filter(
    (rel) => rel === "index.md" || rel.endsWith("/index.md"),
  );
  const documents = [];
  let failed = false;
  for (const rel of pages) {
    try {
      const dir = rel.slice(0, -"index.md".length);
      const url = `/docs/${dir}`;
      documents.push(documentFor(url, readFileSync(`${DOCS}/${rel}`, "utf8")));
    } catch (error) {
      console.error(`${rel}: ${error.message}`);
      failed = true;
    }
  }
  if (failed) return 1;

  const index = new MiniSearch(INDEX_OPTIONS);
  index.addAll(documents);
  const json = JSON.stringify(index);
  writeFileSync(INDEX, json);
  console.log(`indexed ${documents.length} pages (${Buffer.byteLength(json)} bytes)`);
  return 0;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  process.exitCode = main();
}
