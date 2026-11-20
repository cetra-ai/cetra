// Comparison fact files (design §7, §9.5). One JSON file per product in ./compare/.
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

/** The areas, in table order, with their row labels. */
export const AREAS = [
  ["selfHostStack", "What you run to self-host"],
  ["selfHostLicence", "Self-hosted licence"],
  ["cloudFree", "Free cloud tier"],
  ["cloudPaidFrom", "Paid cloud from"],
  ["otel", "OpenTelemetry"],
  ["sql", "SQL over your data"],
  ["mcp", "MCP server"],
  ["onlineEvals", "Evaluations on live traffic"],
  ["datasetsExperiments", "Datasets and experiments"],
  ["promptManagement", "Prompt management"],
  ["gateway", "LLM gateway"],
  ["alerts", "Alerts"],
];

/** The competitors, in page order (design §9.3). */
export const COMPETITORS = [
  "langfuse",
  "phoenix",
  "langsmith",
  "braintrust",
  "helicone",
  "opik",
  "langwatch",
  "laminar",
  "mlflow",
];

/** Files older than this many days fail the build (design §7). */
export const MAX_AGE_DAYS = 183;

// Both `astro build` and `node --test` run with the cwd at website/ (import.meta.dirname is not
// reliable once Vite bundles this file).
const DIR = join(process.cwd(), "src/marketing/data/compare");

/**
 * @param {string} slug
 * @returns {Product}
 */
export function loadProduct(slug) {
  return JSON.parse(readFileSync(join(DIR, `${slug}.json`), "utf8"));
}

export function listFiles() {
  return readdirSync(DIR)
    .filter((f) => f.endsWith(".json"))
    .sort();
}

/**
 * Problems with one product file; empty when valid.
 * @param {any} p @param {string} fileSlug @param {Date} now
 */
export function validate(p, fileSlug, now) {
  const problems = [];
  if (!p || typeof p !== "object") return ["file is not a JSON object"];

  if (p.slug !== fileSlug) problems.push("slug must equal the file name");

  for (const key of ["name", "url", "checkedOn", "oneLine"]) {
    if (typeof p[key] !== "string" || p[key].trim() === "") {
      problems.push("name, url, checkedOn and oneLine are required");
      break;
    }
  }

  if (typeof p.url === "string" && !p.url.startsWith("https://")) {
    problems.push("url must start with https://");
  }

  if (typeof p.oneLine === "string") {
    const length = [...p.oneLine].length;
    if (length > 140) problems.push(`oneLine is ${length} characters, the limit is 140`);
    problems.push(...textProblems("oneLine", p.oneLine));
  }

  if (typeof p.checkedOn === "string") {
    const checked = new Date(`${p.checkedOn}T00:00:00Z`);
    const isDate =
      /^\d{4}-\d{2}-\d{2}$/.test(p.checkedOn) &&
      !Number.isNaN(checked.getTime()) &&
      checked.toISOString().slice(0, 10) === p.checkedOn;
    if (!isDate) {
      problems.push("checkedOn must be YYYY-MM-DD");
    } else if (now.getTime() - checked.getTime() > MAX_AGE_DAYS * 86_400_000) {
      problems.push(
        `checked on ${p.checkedOn}, more than ${MAX_AGE_DAYS} days ago: re-check the facts`,
      );
    }
  } else {
    problems.push("checkedOn must be YYYY-MM-DD");
  }

  const facts = p.facts && typeof p.facts === "object" ? p.facts : {};
  for (const [area] of AREAS) {
    const fact = facts[area];
    if (!fact || typeof fact.value !== "string") {
      problems.push(`${area} missing`);
      continue;
    }
    const length = [...fact.value].length;
    if (length > 120) problems.push(`${area}: value is ${length} characters, the limit is 120`);
    problems.push(...textProblems(area, fact.value));
    if (!Array.isArray(fact.sources) || fact.sources.length === 0) {
      problems.push(`${area}: no source`);
    } else if (
      fact.sources.some(
        (s) => typeof s !== "string" || !(s.startsWith("https://") || s.startsWith("/")),
      )
    ) {
      problems.push(`${area}: source must be https or a cetra.sh path`);
    }
  }
  const known = new Set(AREAS.map(([area]) => area));
  for (const key of Object.keys(facts)) {
    if (!known.has(key)) problems.push(`facts has an unknown area: ${key}`);
  }

  const isStringArray = (list) => Array.isArray(list) && list.every((s) => typeof s === "string");
  if (!isStringArray(p.chooseThem)) {
    problems.push("chooseThem must be an array of strings");
  } else if (p.chooseThem.length > 4) {
    problems.push(`chooseThem has ${p.chooseThem.length} items, 0 to 4 allowed`);
  } else {
    for (const item of p.chooseThem) problems.push(...textProblems("chooseThem", item));
  }

  if (!isStringArray(p.chooseUs)) {
    problems.push("chooseUs must be an array of strings");
  } else if (fileSlug === "cetra") {
    if (p.chooseUs.length > 0) problems.push("chooseUs must be empty");
  } else if (p.chooseUs.length < 1 || p.chooseUs.length > 4) {
    problems.push(`chooseUs has ${p.chooseUs.length} items, 1 to 4 allowed`);
  }
  if (isStringArray(p.chooseUs)) {
    for (const item of p.chooseUs) {
      const length = [...item].length;
      if (length > 120) problems.push(`chooseUs item is ${length} characters, the limit is 120`);
      problems.push(...textProblems("chooseUs", item));
    }
  }

  return problems;
}

/** Banned words and the capitalised product name in one text, as problem messages. */
function textProblems(field, text) {
  const problems = [];
  const banned = /\b(seamless(ly)?|blazing(ly)?|effortless(ly)?|powerful|simply)\b/i.exec(text);
  if (banned) problems.push(`${field}: banned word ${banned[0].toLowerCase()}`);
  if (/\b(Cetra|CETRA)\b/.test(text)) problems.push(`${field}: write cetra in lower case`);
  return problems;
}

/** "Unknown" is rendered as this (its sources stay listed: they are where we looked). */
export const UNKNOWN_TEXT = "Not found in their documentation";

/** "2026-11-20" -> "20 November 2026". @param {string} iso */
export function formatDate(iso) {
  return new Intl.DateTimeFormat("en-GB", {
    day: "numeric",
    month: "long",
    year: "numeric",
    timeZone: "UTC",
  }).format(new Date(`${iso}T00:00:00Z`));
}

/**
 * @typedef {object} Fact
 * @property {string} value
 * @property {string[]} sources
 */

/**
 * @typedef {object} Product
 * @property {string} slug
 * @property {string} name
 * @property {string} url
 * @property {string} checkedOn
 * @property {string} oneLine
 * @property {Record<string, Fact>} facts
 * @property {string[]} chooseThem
 * @property {string[]} chooseUs
 * @property {string} [notes]
 */
