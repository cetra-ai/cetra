// Checks skills/cetra/SKILL.md (D-AO-8): the Agent Skills frontmatter, links to references and docs
// pages, and the `cetra` subcommands it names. Prints `path:line: message` per finding and exits 1
// when there is any.
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { extractStarlightFrontmatter } from "tidymd";

export const SKILL_DIR = "../skills/cetra";
export const PAGES = "src/content/docs/docs";
export const CLI_REFERENCE = "src/content/docs/docs/reference/cli.md";

const MAX_LINES = 500;
const FRONTMATTER_KEYS = new Set(["name", "description", "allowed-tools", "license", "compatibility", "metadata"]);

/** `cetra` subcommand names: the `### cetra <name>` headings of the CLI reference. */
export function cliCommands(cliReference) {
  const names = new Set();
  for (const line of cliReference.split("\n")) {
    const match = line.match(/^### `cetra ([a-z][a-z-]*)`/);
    if (match) names.add(match[1]);
  }
  return names;
}

/** The inline code spans of a line, without their backticks. */
function codeSpans(line) {
  return [...line.matchAll(/`([^`]*)`/g)].map((m) => m[1]);
}

/** The frontmatter's errors. When it is missing or unparsable, that is the only error. */
function checkFrontmatter(text, lines) {
  const errors = [];
  if (!text.startsWith("---\n")) {
    errors.push({ line: 1, message: "frontmatter is missing" });
    return errors;
  }
  const close = lines.findIndex((line, index) => index > 0 && line === "---");
  if (close === -1) {
    errors.push({ line: 1, message: "frontmatter is not closed by ---" });
    return errors;
  }
  let data;
  try {
    data = extractStarlightFrontmatter(text) ?? {};
  } catch (error) {
    errors.push({ line: 1, message: `frontmatter cannot be parsed: ${error.message}` });
    return errors;
  }
  if (data.name !== "cetra") {
    errors.push({ line: 1, message: "name must be cetra, the directory name" });
  }
  const { description } = data;
  if (typeof description !== "string" || description.length < 1 || description.length > 1024) {
    errors.push({ line: 1, message: "description must be a string of 1 to 1024 characters" });
  }
  if ("allowed-tools" in data && typeof data["allowed-tools"] !== "string") {
    errors.push({ line: 1, message: "allowed-tools must be a string" });
  }
  for (const key of Object.keys(data)) {
    if (!FRONTMATTER_KEYS.has(key)) errors.push({ line: 1, message: `unknown frontmatter key ${key}` });
  }
  return errors;
}

/**
 * Returns `{line, message}` errors for SKILL.md's text.
 * ctx = { referenceExists(name: string) => boolean,
 *         pageExists(docsPath: string) => boolean,   // path after `/docs/`, e.g. `integrations/claude-code/`
 *         commands: Set<string> }
 */
export function checkSkill(text, ctx) {
  const lines = text.split("\n");
  const lineCount = text.endsWith("\n") ? lines.length - 1 : lines.length;
  const errors = checkFrontmatter(text, lines);

  if (lineCount > MAX_LINES) {
    errors.push({ line: 1, message: `SKILL.md has ${lineCount} lines; the limit is ${MAX_LINES}` });
  }

  lines.forEach((line, index) => {
    const number = index + 1;
    for (const [, target] of line.matchAll(/\]\(([^)\s]+)\)/g)) {
      const reference = target.match(/^references\/([a-z0-9-]+)\.md$/);
      if (reference) {
        if (!ctx.referenceExists(reference[1])) {
          errors.push({ line: number, message: `missing reference references/${reference[1]}.md` });
        }
      } else if (!/^(https?:\/\/|#)/.test(target)) {
        errors.push({
          line: number,
          message: `link ${target} must be references/<name>.md or an absolute URL`,
        });
      }
    }
    for (const [, raw] of line.matchAll(/https:\/\/cetra\.sh\/docs\/([^#)`\s]*)/g)) {
      const path = raw.replace(/[.,;:]+$/, "");
      if (!path.endsWith("/") || !ctx.pageExists(path)) {
        errors.push({ line: number, message: `no docs page for https://cetra.sh/docs/${path}` });
      }
    }
  });

  // Commands count only in code: inline code spans and the lines of fenced blocks.
  let fence = null;
  lines.forEach((line, index) => {
    const trimmed = line.trim();
    let codeTexts;
    if (fence) {
      if (trimmed.length >= fence.length && /^`+$/.test(trimmed)) fence = null;
      codeTexts = [line];
    } else {
      const opening = trimmed.match(/^`{3,}/);
      if (opening) {
        fence = opening[0];
        return;
      }
      codeTexts = codeSpans(line);
    }
    for (const code of codeTexts) {
      for (const [, name] of code.matchAll(/(?:^|[\s`(])cetra ([a-z][a-z-]*)/g)) {
        if (!ctx.commands.has(name)) {
          errors.push({ line: index + 1, message: `unknown command \`cetra ${name}\`` });
        }
      }
    }
  });

  return errors;
}

/** The checker's context on disk: the references, the docs pages and the CLI commands. */
export function realContext() {
  return {
    referenceExists: (name) => existsSync(join(SKILL_DIR, "references", `${name}.md`)),
    pageExists: (path) =>
      [
        join(PAGES, `${path.slice(0, -1)}.mdx`),
        join(PAGES, `${path.slice(0, -1)}.md`),
        join(PAGES, path, "index.mdx"),
        join(PAGES, path, "index.md"),
      ].some((candidate) => existsSync(candidate)),
    commands: cliCommands(readFileSync(CLI_REFERENCE, "utf8")),
  };
}

export function main() {
  let text;
  try {
    text = readFileSync(join(SKILL_DIR, "SKILL.md"), "utf8");
  } catch {
    console.error("skills/cetra/SKILL.md: missing");
    return 1;
  }
  const ctx = realContext();
  if (ctx.commands.size === 0) {
    console.error("website/src/content/docs/docs/reference/cli.md: no command headings found");
    return 1;
  }

  const problems = [];
  for (const entry of readdirSync(SKILL_DIR, { withFileTypes: true })) {
    if (entry.name === "SKILL.md" && entry.isFile()) continue;
    if (entry.name === "references" && entry.isDirectory()) {
      for (const file of readdirSync(join(SKILL_DIR, "references"), { withFileTypes: true })) {
        if (!(file.isFile() && file.name.endsWith(".md"))) {
          problems.push(`skills/cetra/references/${file.name}: not part of the skill`);
        }
      }
      continue;
    }
    problems.push(`skills/cetra/${entry.name}: not part of the skill`);
  }

  for (const error of checkSkill(text, ctx)) {
    problems.push(`skills/cetra/SKILL.md:${error.line}: ${error.message}`);
  }
  for (const problem of problems) console.error(problem);
  return problems.length === 0 ? 0 : 1;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  process.exit(main());
}
