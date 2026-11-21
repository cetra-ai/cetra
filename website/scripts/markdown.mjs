// Converts a docs page (MDX) to plain Markdown for agents: the skill references (D-AO-8) and the
// site's `.md` pages (M4). tidymd does the Starlight components; the pre-pass below handles ours.
import { cleanStarlightMarkdown, extractStarlightFrontmatter } from "tidymd";

/** The site's origin, used to make `/docs/...` links absolute. */
export const SITE = "https://cetra.sh";

const FRONT_MATTER = "---";
const PROSE = 0;
const FENCE = 1; // a fence's opening or closing line
const CODE = 2; // a line between the fence's lines

/** The backtick run that opens a fence on this trimmed line, or 0. */
function fenceRun(trimmed) {
  const match = /^`{3,}/.exec(trimmed);
  return match ? match[0].length : 0;
}

/** True when the trimmed line closes a fence opened by a run of `run` backticks. */
function closesFence(trimmed, run) {
  return /^`+$/.test(trimmed) && trimmed.length >= run;
}

/** The kind of each line: PROSE, FENCE or CODE. */
function fenceKinds(lines) {
  let fence = 0;
  return lines.map((line) => {
    const trimmed = line.trim();
    if (fence) {
      if (closesFence(trimmed, fence)) {
        fence = 0;
        return FENCE;
      }
      return CODE;
    }
    const run = fenceRun(trimmed);
    if (run) {
      fence = run;
      return FENCE;
    }
    return PROSE;
  });
}

/**
 * Our components and markers, removed or rewritten before tidymd runs.
 * Lines inside fenced code blocks are never touched.
 * @param {string} source the page's full text, front matter included
 * @returns {string}
 */
export function prepass(source) {
  const lines = source.split("\n");
  const out = [];
  let i = 0;
  if (lines[0] === FRONT_MATTER) {
    const close = lines.indexOf(FRONT_MATTER, 1);
    if (close !== -1) {
      out.push(...lines.slice(0, close + 1));
      i = close + 1;
    }
  }
  let fence = 0; // the backtick run that opens the current fence, or 0
  while (i < lines.length) {
    const line = lines[i];
    const trimmed = line.trim();
    const lineNo = i + 1;
    if (fence) {
      if (closesFence(trimmed, fence)) fence = 0;
      out.push(line);
      i += 1;
      continue;
    }
    const run = fenceRun(trimmed);
    if (run) {
      fence = run;
      out.push(line);
      i += 1;
      continue;
    }
    if (trimmed.startsWith("~~~")) {
      throw new Error(`line ${lineNo}: use backtick fences`);
    }
    if (/^\{\/\*[\s\S]*\*\/\}$/.test(trimmed)) {
      i += 1;
      continue;
    }
    if (
      /^<Tabs\b[^>]*>$/.test(trimmed) ||
      trimmed === "</Tabs>" ||
      trimmed === "</TabItem>"
    ) {
      i += 1;
      continue;
    }
    const tabItem = /^<TabItem\b[^>]*\blabel="([^"]+)"[^>]*>$/.exec(trimmed);
    if (tabItem) {
      out.push("", `**${tabItem[1]}**`, "");
      i += 1;
      continue;
    }
    if (trimmed.startsWith("<Backing") || trimmed.startsWith("<Image")) {
      let end = i;
      while (end < lines.length && !lines[end].trim().endsWith("/>")) end += 1;
      if (end === lines.length) {
        throw new Error(`unclosed <Backing> or <Image> at line ${lineNo}`);
      }
      i = end + 1;
      continue;
    }
    if (/^<CardGrid\b[^>]*>$/.test(trimmed) || trimmed === "</CardGrid>") {
      i += 1;
      continue;
    }
    const card = /^<LinkCard\b[^>]*\btitle="([^"]+)"[^>]*\bhref="([^"]+)"[^>]*\/>$/.exec(trimmed);
    if (card) {
      const description = /\bdescription="([^"]+)"/.exec(trimmed);
      out.push(`- [${card[1]}](${card[2]})${description ? `: ${description[1]}` : ""}`);
      i += 1;
      continue;
    }
    out.push(line);
    i += 1;
  }
  return out.join("\n");
}

/**
 * Trailing spaces go, leading spaces go before headings and fences outside code, runs of blank
 * lines outside code become one, and the text ends with one newline.
 * @param {string} text
 * @returns {string}
 */
function postPass(text) {
  const lines = text.split("\n");
  const kinds = fenceKinds(lines);
  const out = [];
  let previousBlank = false;
  lines.forEach((raw, i) => {
    const kind = kinds[i];
    let line = raw.trimEnd();
    if (kind === CODE) {
      out.push(line);
      previousBlank = false;
      return;
    }
    if (kind === FENCE || line.trim().startsWith("#")) line = line.trimStart();
    if (kind === PROSE && line === "") {
      if (previousBlank) return;
      previousBlank = true;
    } else {
      previousBlank = false;
    }
    out.push(line);
  });
  return `${out.join("\n").replace(/\n+$/, "")}\n`;
}

/**
 * Throws when a JSX component survives outside code. Inline code spans are ignored, so
 * `<YOUR_KEY>` placeholders pass.
 * @param {string} text
 */
function assertNoComponent(text) {
  const lines = text.split("\n");
  const kinds = fenceKinds(lines);
  lines.forEach((line, i) => {
    if (kinds[i] !== PROSE) return;
    const match = /<\/?[A-Z][a-z][A-Za-z]*\b/.exec(line.replace(/`[^`]*`/g, ""));
    if (match) throw new Error(`line ${i + 1}: unconverted component ${match[0]}`);
  });
}

/**
 * The page as Markdown: `# <title>`, the description as the first paragraph,
 * then the body. Links to `/docs/...` become `${SITE}/docs/...`.
 * Throws an Error naming the line when a JSX component survives the conversion.
 * @param {string} source
 * @returns {string}
 */
export function toMarkdown(source) {
  const frontmatter = extractStarlightFrontmatter(source);
  let body = cleanStarlightMarkdown(prepass(source), {
    frontmatter: "title-as-heading",
    internalLinks: { mode: "baseUrl", baseUrl: SITE },
  });
  const description = frontmatter?.description;
  if (typeof description === "string" && description.trim() !== "") {
    const lines = body.split("\n");
    const heading = lines.findIndex((line) => line.startsWith("# "));
    if (heading === -1) {
      lines.unshift(description, "");
    } else {
      lines.splice(heading + 1, 0, "", description, "");
    }
    body = lines.join("\n");
  }
  body = postPass(body);
  assertNoComponent(body);
  return body;
}
