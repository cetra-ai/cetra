// Checks every `CETRA_*` name in the docs content (D-W-7): a plain name must occur in the code,
// a split name (`CETRA_STORAGE__FLUSH__MAX_AGE`) must be a documented configuration key, and a
// pattern (`CETRA_AUTH__OIDC__<NAME>__CLIENT_SECRET`) must start like a name in the code.
// Prints `path:line: message` per finding and exits 1 when there is any.
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const SKIPPED = new Set(["node_modules", "target", ".git", "dist"]);

/** The finding for one name, or null. `before` is the character after the name. */
function problem(name, next, codeText, configKeys) {
  if (next === "<") {
    return codeText.includes(name) ? null : `${name}<…> is not read by the code`;
  }
  const trimmed = name.replace(/_+$/, "");
  if (trimmed === "CETRA") return null; // the bare prefix, as in "`CETRA_` followed by …"
  if (trimmed.includes("__")) {
    const key = trimmed.slice("CETRA_".length).toLowerCase().replaceAll("__", ".");
    return configKeys.has(key) ? null : `${trimmed} is not a documented configuration key`;
  }
  return codeText.includes(trimmed) ? null : `${trimmed} is not read by the code`;
}

/** `pages` maps a path to its text; fenced code counts. */
export function checkSite(pages, codeText, configKeys) {
  const findings = [];
  for (const [path, text] of Object.entries(pages)) {
    text.split("\n").forEach((line, index) => {
      for (const match of line.matchAll(/\bCETRA_[A-Z0-9_]*/g)) {
        const next = line[match.index + match[0].length];
        const message = problem(match[0], next, codeText, configKeys);
        if (message) findings.push(`${path}:${index + 1}: ${message}`);
      }
    });
  }
  return findings;
}

function walk(dir, accept, into) {
  let entries;
  try {
    entries = readdirSync(dir);
  } catch {
    return;
  }
  for (const entry of entries) {
    if (SKIPPED.has(entry)) continue;
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) walk(path, accept, into);
    else if (accept(path)) into.push(path);
  }
}

function readAll(paths) {
  return paths.map((path) => readFileSync(path, "utf8")).join("\n");
}

export function main(root) {
  const pagePaths = [];
  walk(join(root, "website/src/content/docs"), (p) => /\.mdx?$/.test(p), pagePaths);
  const pages = Object.fromEntries(pagePaths.map((p) => [p, readFileSync(p, "utf8")]));

  const codePaths = [];
  for (const crate of readdirSync(join(root, "crates"))) {
    walk(join(root, "crates", crate, "src"), () => true, codePaths);
  }
  for (const dir of ["sdks/python/src", "sdks/typescript/src", "deploy", "tools/packaging"]) {
    walk(join(root, dir), () => true, codePaths);
  }
  const codeText = readAll(codePaths);

  const reference = join(
    root,
    "website/src/content/docs/docs/self-hosting/configuration-reference.md",
  );
  const configKeys = new Set();
  for (const line of readFileSync(reference, "utf8").split("\n")) {
    const m = line.match(/^\| `([^`]+)`/);
    if (m) configKeys.add(m[1]);
  }

  const findings = checkSite(pages, codeText, configKeys);
  for (const f of findings) console.error(f);
  return findings.length === 0 ? 0 : 1;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  process.exit(main(join(fileURLToPath(import.meta.url), "../../..")));
}
