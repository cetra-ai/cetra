// Checks every built marketing page (design §7, M1 plan F4). Run after `astro build`.
// Prints `route: message` per finding and exits 1 when there is any.
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { isNoindex, LAUNCHED, MARKETING_ROUTES } from "../src/marketing/routes.mjs";
import { SITE_URL } from "../src/marketing/site.ts";

const DIST = join(dirname(fileURLToPath(import.meta.url)), "..", "dist");
const MAX_PAGE_BYTES = 350_000;
const MAX_DESCRIPTION = 160;
const BANNED_WORD = /\b(seamless(ly)?|blazing(ly)?|effortless(ly)?|powerful|simply)\b/gi;
const CETRA_CAPS = /\b(Cetra|CETRA)\b/g;

const NAMED_ENTITIES = { amp: "&", lt: "<", gt: ">", quot: '"' };

/** Decodes the entities that can appear in attribute values and text. */
export function decode(text) {
  return text.replace(/&(#x[\da-f]+|#\d+|[a-z]+);/gi, (match, entity) => {
    if (entity[0] === "#") {
      const code =
        entity[1].toLowerCase() === "x"
          ? Number.parseInt(entity.slice(2), 16)
          : Number(entity.slice(1));
      return code <= 0x10ffff ? String.fromCodePoint(code) : match;
    }
    return NAMED_ENTITIES[entity.toLowerCase()] ?? match;
  });
}

/** Removes script and style contents and comments, so their text is not read as markup. */
export function stripBlocks(html) {
  return html
    .replace(/<script\b[^>]*>[\s\S]*?<\/script>/gi, "")
    .replace(/<style\b[^>]*>[\s\S]*?<\/style>/gi, "")
    .replace(/<!--[\s\S]*?-->/g, "");
}

/** Parses an attribute string into a Map. Names are lower-cased; a bare attribute is "". */
function attributes(source) {
  const map = new Map();
  for (const m of source.matchAll(/([^\s=/>]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+)))?/g)) {
    const value = m[2] ?? m[3] ?? m[4] ?? "";
    map.set(m[1].toLowerCase(), decode(value));
  }
  return map;
}

/** The attributes of every opening `<name ...>` tag, in document order. */
export function tags(html, name) {
  return [...html.matchAll(new RegExp(`<${name}\\b([^>]*)>`, "gi"))].map((m) => attributes(m[1]));
}

/** Every `<name ...>inner</name>` element, with its attributes and inner HTML. */
export function elements(html, name) {
  return [...html.matchAll(new RegExp(`<${name}\\b([^>]*)>([\\s\\S]*?)</${name}>`, "gi"))].map(
    (m) => ({ attrs: attributes(m[1]), inner: m[2] }),
  );
}

/** The visible text: tags become spaces, entities are decoded, whitespace is collapsed. */
export function textOf(inner) {
  return decode(inner.replace(/<[^>]*>/g, " "))
    .replace(/\s+/g, " ")
    .trim();
}

/** True when an element has visible text, an aria-label, or an img with alt text. */
function isNamed(element) {
  if (textOf(element.inner) !== "") return true;
  if ((element.attrs.get("aria-label") ?? "").trim() !== "") return true;
  return tags(element.inner, "img").some((img) => (img.get("alt") ?? "") !== "");
}

/**
 * The findings for one built page, as messages (without the route).
 * `distFiles` holds the built paths, e.g. "/docs/index.html".
 */
export function checkPage(route, html, { noindex, distFiles = new Set() }) {
  const findings = [];
  const stripped = stripBlocks(html);

  if (tags(stripped, "html")[0]?.get("lang") !== "en") findings.push('missing lang="en"');

  const title = elements(stripped, "title")[0];
  if (!title || textOf(title.inner) === "") findings.push("title missing");

  const metas = tags(stripped, "meta");
  const description = metas.find((m) => m.get("name") === "description")?.get("content") ?? "";
  if (description.trim() === "") {
    findings.push("description missing");
  } else if ([...description].length > MAX_DESCRIPTION) {
    findings.push(
      `description is ${[...description].length} characters, the limit is ${MAX_DESCRIPTION}`,
    );
  }

  const expectedCanonical = `${SITE_URL}${route}`;
  const canonical = tags(stripped, "link").find((l) =>
    (l.get("rel") ?? "").split(/\s+/).includes("canonical"),
  );
  if (canonical?.get("href") !== expectedCanonical) {
    findings.push(`canonical must be ${expectedCanonical}`);
  }

  const robots = metas.some(
    (m) => m.get("name") === "robots" && (m.get("content") ?? "").includes("noindex"),
  );
  if (noindex && !robots) findings.push("noindex expected");
  if (!noindex && robots) findings.push("noindex not allowed");

  const levels = [...stripped.matchAll(/<h([1-6])\b/gi)].map((m) => Number(m[1]));
  const h1Count = levels.filter((level) => level === 1).length;
  if (h1Count !== 1) findings.push(`expected one h1, found ${h1Count}`);
  for (let i = 1; i < levels.length; i++) {
    if (levels[i] > levels[i - 1] + 1) {
      findings.push(`heading level skipped: h${levels[i - 1]} then h${levels[i]}`);
    }
  }

  for (const img of tags(stripped, "img")) {
    if (!img.has("alt")) findings.push("image without alt");
  }

  for (const link of elements(stripped, "a")) {
    if (!isNamed(link)) findings.push(`link without a name: ${link.attrs.get("href") ?? ""}`);
  }
  for (const button of elements(stripped, "button")) {
    if (!isNamed(button)) findings.push("button without a name");
  }

  for (const a of tags(stripped, "a")) {
    const href = a.get("href");
    if (href === "#") findings.push('href="#" not allowed');
    if (href === undefined || !href.startsWith("/") || href.startsWith("//")) continue;
    // The holding page at / has no anchors until launch.
    if (!LAUNCHED && (href === "/" || href.startsWith("/#"))) continue;
    const path = href.replace(/[?#].*$/, "");
    const built = path.endsWith("/") ? distFiles.has(`${path}index.html`) : distFiles.has(path);
    if (!built) findings.push(`broken internal link: ${href}`);
  }

  const bodyHtml = elements(stripped, "body")[0]?.inner ?? stripped;
  const text = textOf(bodyHtml);
  for (const m of text.matchAll(BANNED_WORD)) findings.push(`banned word: ${m[0]}`);
  findings.push(...Array.from(text.matchAll(CETRA_CAPS), () => "write cetra in lower case"));

  return findings;
}

/** The bytes a visitor downloads: the HTML and the /_astro/ stylesheets and scripts. Fonts are not counted. */
export function pageWeight(html, readAsset) {
  let bytes = Buffer.byteLength(html);
  for (const link of tags(html, "link")) {
    const href = link.get("href") ?? "";
    if (
      (link.get("rel") ?? "").split(/\s+/).includes("stylesheet") &&
      href.startsWith("/_astro/")
    ) {
      bytes += readAsset(href).length;
    }
  }
  for (const script of tags(html, "script")) {
    const src = script.get("src") ?? "";
    if (src.startsWith("/_astro/")) bytes += readAsset(src).length;
  }
  return bytes;
}

/** The routes of the built pages outside /docs/. The holding page's "/" is left out until launch. */
export function listedRoutes(distFiles, launched) {
  return [...distFiles]
    .filter((file) => file.endsWith("/index.html") && !file.startsWith("/docs/"))
    .map((file) => file.slice(0, -"index.html".length))
    .filter((route) => launched || route !== "/");
}

/** Every built file as a path with a leading "/". */
function walk(dir) {
  const files = new Set();
  for (const entry of readdirSync(dir, { recursive: true })) {
    const path = join(dir, entry);
    if (statSync(path).isFile()) files.add(`/${entry.split(sep).join("/")}`);
  }
  return files;
}

function run() {
  const distFiles = walk(DIST);
  const findings = [];

  for (const route of listedRoutes(distFiles, LAUNCHED)) {
    if (!MARKETING_ROUTES.includes(route)) findings.push(`${route}: unlisted marketing page`);
  }

  const readAsset = (path) => readFileSync(join(DIST, path));
  for (const route of MARKETING_ROUTES) {
    const file = join(DIST, route, "index.html");
    if (!existsSync(file)) {
      findings.push(`${route}: page not built`);
      continue;
    }
    const html = readFileSync(file, "utf8");
    for (const message of checkPage(route, html, { noindex: isNoindex(route), distFiles })) {
      findings.push(`${route}: ${message}`);
    }
    const weight = pageWeight(html, readAsset);
    if (weight > MAX_PAGE_BYTES) {
      findings.push(`${route}: page weighs ${weight} bytes, the cap is ${MAX_PAGE_BYTES}`);
    }
  }

  if (findings.length > 0) {
    for (const finding of findings) console.error(finding);
    return 1;
  }
  console.log(`check-marketing: ${MARKETING_ROUTES.length} pages ok`);
  return 0;
}

if (import.meta.main) {
  process.exitCode = run();
}
