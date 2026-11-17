// Sample-marker check for the docs content (website/STYLE.md, D-W-6).
// Every python/ts/tsx/js fence needs a test marker or an `untested` marker with a reason.
// Prints `path:line: message` per finding and exits 1 when there is any.
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const SAMPLE_LANGUAGES = new Set(["python", "py", "ts", "typescript", "tsx", "js", "javascript"]);
const PYTHON = new Set(["python", "py"]);
const TS = new Set(["ts", "typescript"]);

export function checkFile(path, text) {
  const findings = [];
  const lines = text.split("\n");
  let fence = null; // the opening fence marker while inside a fence
  for (let i = 0; i < lines.length; i++) {
    const open = lines[i].match(/^\s*(`{3,}|~{3,})\s*(\S*)/);
    if (!open) continue;
    if (fence !== null) {
      if (open[1][0] === fence[0] && open[1].length >= fence.length && open[2] === "") fence = null;
      continue;
    }
    fence = open[1];
    const language = open[2].toLowerCase();
    if (!SAMPLE_LANGUAGES.has(language)) continue;
    let j = i - 1;
    while (j >= 0 && lines[j].trim() === "") j--;
    const marker = j >= 0 ? lines[j].trim().match(/^\{\/\*\s*(.*?)\s*\*\/\}$/) : null;
    const where = `${path}:${i + 1}`;
    if (!marker) {
      findings.push(`${where}: ${language} block without a test marker (D-W-6)`);
      continue;
    }
    const body = marker[1];
    if (body.startsWith("untested:")) {
      if (body.slice("untested:".length).trim() === "") {
        findings.push(`${where}: untested marker without a reason (D-W-6)`);
      }
    } else if (body === "test" || body === "test:otel") {
      if (!PYTHON.has(language)) {
        findings.push(`${where}: ${body} marker before a ${language} block, expected python`);
      }
    } else if (/^example:[a-z0-9-]+$/.test(body)) {
      if (!TS.has(language)) {
        findings.push(`${where}: example marker before a ${language} block, expected ts`);
      }
    } else {
      findings.push(`${where}: unknown marker {/* ${body} */} before a ${language} block`);
    }
  }
  return findings;
}

export function main(root) {
  const findings = [];
  for (const entry of readdirSync(root, { recursive: true })) {
    if (!/\.mdx?$/.test(entry)) continue;
    const path = join(root, entry);
    findings.push(...checkFile(path, readFileSync(path, "utf8")));
  }
  for (const f of findings) console.error(f);
  return findings.length === 0 ? 0 : 1;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  process.exit(main("src/content/docs"));
}
