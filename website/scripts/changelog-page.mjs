// Writes the Changelog page from CHANGELOG.md at build time (D-W-7).
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const REPO = "https://github.com/cetra-ai/cetra/blob/main/";

/** The docs page for a changelog: frontmatter, a generated notice, the body without its title. */
export function changelogPage(markdown) {
  const body = markdown
    .replace(/^# Changelog\s*\n/, "")
    .replace(/\]\((?!https?:|#|\/|mailto:)([^)]+)\)/g, (_, path) => `](${REPO}${path})`)
    .trim();
  return [
    "---",
    "title: Changelog",
    "description: Every user-visible change to cetra, release by release.",
    "---",
    "",
    "<!-- Generated from CHANGELOG.md by scripts/changelog-page.mjs. Do not edit. -->",
    "",
    body,
    "",
  ].join("\n");
}

export function main() {
  const source = readFileSync(new URL("../../CHANGELOG.md", import.meta.url), "utf8");
  const target = new URL("../src/content/docs/docs/help/changelog.md", import.meta.url);
  writeFileSync(target, changelogPage(source));
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  main();
}
