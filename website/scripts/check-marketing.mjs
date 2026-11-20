// Checks every built marketing page (design §7, M1 plan F4). Run after `astro build`.
// Prints `route: message` per finding and exits 1 when there is any.
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { ALWAYS_NOINDEX, isNoindex, LAUNCHED, MARKETING_ROUTES } from "../src/marketing/routes.mjs";
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

/**
 * Removes script and style contents and comments, so their text is not read as markup. This only
 * reads our own build output; it repeats until nothing changes, so a removal cannot leave a new
 * block behind, and accepts end tags with whitespace before ">".
 */
export function stripBlocks(html) {
  let out = html;
  let previous;
  do {
    previous = out;
    out = out
      .replace(/<script\b[^>]*>[\s\S]*?<\/script\s*>/gi, "")
      .replace(/<style\b[^>]*>[\s\S]*?<\/style\s*>/gi, "")
      .replace(/<!--[\s\S]*?-->/g, "");
  } while (out !== previous);
  return out;
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

/** The attributes of every opening tag, in document order. */
function allAttributes(html) {
  return [...html.matchAll(/<[a-z][^\s/>]*([^>]*)>/gi)].map((m) => attributes(m[1]));
}

/** The visible text of the element with this id, or "" when there is none. */
function textOfId(html, id) {
  const escaped = id.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const m = html.match(
    new RegExp(`<([a-z][a-z0-9]*)\\b[^>]*\\sid="${escaped}"[^>]*>([\\s\\S]*?)</\\1>`, "i"),
  );
  return m ? textOf(m[2]) : "";
}

/**
 * True when an element has visible text, an aria-label, an aria-labelledby that points at text,
 * an inner svg with an aria-label or a title, or an img with alt text.
 */
function isNamed(element, html) {
  if (textOf(element.inner) !== "") return true;
  if ((element.attrs.get("aria-label") ?? "").trim() !== "") return true;
  const labelledBy = (element.attrs.get("aria-labelledby") ?? "").split(/\s+/).filter(Boolean);
  if (labelledBy.some((id) => textOfId(html, id) !== "")) return true;
  const svgs = elements(element.inner, "svg");
  if (svgs.some((svg) => (svg.attrs.get("aria-label") ?? "").trim() !== "")) return true;
  if (svgs.some((svg) => elements(svg.inner, "title").some((t) => textOf(t.inner) !== ""))) {
    return true;
  }
  return tags(element.inner, "img").some((img) => (img.get("alt") ?? "") !== "");
}

/**
 * The findings for one built page, as messages (without the route).
 * `distFiles` holds the built paths, e.g. "/docs/index.html".
 */
export function checkPage(route, html, { noindex, distFiles = new Set(), launched = LAUNCHED }) {
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
    if (!isNamed(link, stripped))
      findings.push(`link without a name: ${link.attrs.get("href") ?? ""}`);
  }
  for (const button of elements(stripped, "button")) {
    if (!isNamed(button, stripped)) findings.push("button without a name");
  }

  for (const a of tags(stripped, "a")) {
    const href = a.get("href");
    if (href === "#") findings.push('href="#" not allowed');
    if (href === undefined || !href.startsWith("/") || href.startsWith("//")) continue;
    // Before launch, / is the holding page, which has no anchors: its links are skipped.
    // From launch on, / is the homepage and its links are checked like any other.
    if (!launched && (href === "/" || href.startsWith("/#"))) continue;
    const path = href.replace(/[?#].*$/, "");
    const built = path.endsWith("/") ? distFiles.has(`${path}index.html`) : distFiles.has(path);
    if (!built) findings.push(`broken internal link: ${href}`);
  }

  // Every id a reference points at must exist on the page: aria-labelledby and aria-controls
  // tokens, and in-page links. Tabs set aria-controls at runtime, so it is absent from the HTML.
  const ids = new Set(
    allAttributes(stripped)
      .map((a) => a.get("id"))
      .filter(Boolean),
  );
  const targets = new Set();
  for (const a of allAttributes(stripped)) {
    for (const attribute of ["aria-labelledby", "aria-controls"]) {
      for (const token of (a.get(attribute) ?? "").split(/\s+/)) {
        if (token !== "") targets.add(token);
      }
    }
  }
  for (const a of tags(stripped, "a")) {
    const href = a.get("href") ?? "";
    if (href.startsWith("#") && href.length > 1) targets.add(href.slice(1));
  }
  for (const target of targets) {
    if (!ids.has(target)) findings.push(`missing id target: ${target}`);
  }

  // Everything a reader or a screen reader hears: the body text, the title, the description,
  // every alt and every aria-label.
  const bodyHtml = elements(stripped, "body")[0]?.inner ?? stripped;
  const scanned = [
    textOf(bodyHtml),
    title ? textOf(title.inner) : "",
    description,
    ...tags(stripped, "img").map((img) => img.get("alt") ?? ""),
    ...allAttributes(stripped).map((a) => a.get("aria-label") ?? ""),
  ].join(" ");
  for (const m of scanned.matchAll(BANNED_WORD)) findings.push(`banned word: ${m[0]}`);
  findings.push(...Array.from(scanned.matchAll(CETRA_CAPS), () => "write cetra in lower case"));

  const hasOgImage = metas.some(
    (m) => m.get("property") === "og:image" && (m.get("content") ?? "").startsWith(`${SITE_URL}/`),
  );
  if (!hasOgImage) findings.push("og:image missing");
  if (!distFiles.has("/og.png")) findings.push("og image /og.png not built");

  return findings;
}

/**
 * The findings for dist/sitemap-0.xml (without the route). A noindex page must not be listed.
 * When `launched`, every indexable marketing route must be listed.
 */
export function checkSitemap(xml, launched) {
  const findings = [];
  const listed = (route) => xml.includes(`<loc>https://cetra.sh${route}</loc>`);
  for (const route of ALWAYS_NOINDEX) {
    if (listed(route)) findings.push(`sitemap lists noindex page ${route}`);
  }
  if (launched) {
    for (const route of MARKETING_ROUTES) {
      if (!ALWAYS_NOINDEX.includes(route) && !listed(route)) {
        findings.push(`sitemap misses ${route}`);
      }
    }
  }
  return findings;
}

/** The bytes a visitor downloads: the HTML and the /_astro/ stylesheets, scripts and module preloads. Fonts are not counted. */
export function pageWeight(html, readAsset) {
  let bytes = Buffer.byteLength(html);
  for (const link of tags(html, "link")) {
    const rel = (link.get("rel") ?? "").split(/\s+/);
    const path = assetPath(link.get("href"));
    if (
      (rel.includes("stylesheet") || rel.includes("modulepreload")) &&
      path.startsWith("/_astro/")
    ) {
      bytes += readAsset(path).length;
    }
  }
  for (const script of tags(html, "script")) {
    const path = assetPath(script.get("src"));
    if (path.startsWith("/_astro/")) bytes += readAsset(path).length;
  }
  return bytes;
}

/** The path of a local asset, without its ?query or #fragment. */
function assetPath(href = "") {
  return href.replace(/[?#].*$/, "");
}

/** The routes of the built pages outside /docs/. Before launch "/" is the holding page and is left out. */
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

  if (!distFiles.has("/sitemap-index.xml")) {
    findings.push("sitemap-index.xml not built");
  }
  if (distFiles.has("/sitemap-0.xml")) {
    const xml = readFileSync(join(DIST, "sitemap-0.xml"), "utf8");
    for (const message of checkSitemap(xml, LAUNCHED)) findings.push(`sitemap: ${message}`);
  } else {
    findings.push("sitemap-0.xml not built");
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
