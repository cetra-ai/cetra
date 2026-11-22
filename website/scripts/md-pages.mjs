// Writes a Markdown copy of every docs page next to its HTML after `astro build` (D-AO-12):
// `src/content/docs/docs/<p>.mdx` → `dist/docs/<p>/index.md`.
import { existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { toMarkdown } from "./markdown.mjs";

export const SOURCE = "src/content/docs/docs";
export const OUT = "dist/docs";

/** "reference/cli.md" → "reference/cli/index.md"; "index.mdx" → "index.md"; "evaluation/index.mdx" → "evaluation/index.md" */
export function outputPath(relative) {
  const stem = relative.replace(/\.mdx?$/, "");
  let dir = stem;
  if (stem === "index") dir = "";
  else if (stem.endsWith("/index")) dir = stem.slice(0, -"/index".length);
  return dir === "" ? "index.md" : `${dir}/index.md`;
}

/** Converts every docs page; returns 0 when all of them converted, 1 otherwise. */
export function main() {
  if (!existsSync(OUT)) {
    console.error(`${OUT} is missing: run astro build first`);
    return 1;
  }
  let failed = false;
  let written = 0;
  const pages = readdirSync(SOURCE, { recursive: true }).filter((rel) => /\.mdx?$/.test(rel));
  for (const rel of pages) {
    try {
      const markdown = toMarkdown(readFileSync(join(SOURCE, rel), "utf8"));
      const target = join(OUT, outputPath(rel));
      mkdirSync(dirname(target), { recursive: true });
      writeFileSync(target, markdown);
      written += 1;
    } catch (error) {
      console.error(`${rel}: ${error.message}`);
      failed = true;
    }
  }
  console.log(`wrote ${written} Markdown pages`);
  return failed ? 1 : 0;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  process.exitCode = main();
}
