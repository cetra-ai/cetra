// Terminology and frontmatter check for the docs content (website/STYLE.md, D-W-15).
// Prints `path:line: message` per finding and exits 1 when there is any.
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const BANNED = [
  [/\bllmobs\b/i, "the old name (D46)"],
  [/\b(Cetra|CETRA)\b/, "cetra is lower case", true],
  [/\b(seamless(ly)?|blazing(ly)?|effortless(ly)?|powerful)\b/i, "marketing word"],
  [/\bsimply\b/i, "remove 'simply'"],
  [/\bOpen Telemetry\b|\botel\b/, "write OpenTelemetry or OTel"],
  [/\bon-prem(ise)?\b/i, "write self-hosted"],
];

const MAX_DESCRIPTION = 160;

function stripInlineCode(line) {
  return line.replace(/`[^`]*`/g, "");
}

export function checkFile(path, text) {
  const findings = [];
  const lines = text.split("\n");
  let inFence = false;
  let frontmatterEnd = 0;
  let description = null;
  if (lines[0]?.trim() === "---") {
    for (let i = 1; i < lines.length; i++) {
      if (lines[i].trim() === "---") {
        frontmatterEnd = i + 1;
        break;
      }
      const m = lines[i].match(/^description:\s*(.*)$/);
      if (m) {
        description = m[1].trim().replace(/^(["'])(.*)\1$/, "$2");
      }
    }
  }
  if (!description) {
    findings.push(`${path}:1: missing description in the frontmatter`);
  } else if (description.length > MAX_DESCRIPTION) {
    findings.push(
      `${path}:1: description is ${description.length} characters, the limit is ${MAX_DESCRIPTION}`,
    );
  }
  for (let i = frontmatterEnd; i < lines.length; i++) {
    const line = lines[i];
    if (/^\s*```/.test(line)) {
      inFence = !inFence;
      continue;
    }
    if (inFence) continue;
    // Sample markers such as `{/* test:otel */}` are not prose.
    if (/^\{\/\*.*\*\/\}$/.test(line.trim())) continue;
    const prose = stripInlineCode(line);
    for (const [pattern, message] of BANNED) {
      if (pattern.test(prose)) findings.push(`${path}:${i + 1}: ${message}`);
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
