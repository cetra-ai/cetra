// Reads `pnpm licenses list --prod --json` on stdin and prints a Markdown section with each
// package of the web UI and its licence text (D-N-8). Used by tools/packaging/notices.sh.
//
// pnpm prints an object keyed by licence id; each value is a list of packages with `name`,
// `versions` (list) and `paths` (list, the installed package directories).
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const LICENSE_FILE = /^(licen[cs]e|copying|notice)([-._][^/]*)?$/i;

function licenseTexts(dir) {
  let names;
  try {
    names = readdirSync(dir);
  } catch {
    return [];
  }
  return names
    .filter((name) => LICENSE_FILE.test(name))
    .sort()
    .map((name) => readFileSync(join(dir, name), "utf8").trim());
}

const input = JSON.parse(readFileSync(0, "utf8"));
const packages = [];
for (const [license, list] of Object.entries(input)) {
  for (const pkg of list) {
    packages.push({
      name: pkg.name,
      versions: pkg.versions ?? [pkg.version],
      paths: pkg.paths ?? [pkg.path],
      license,
    });
  }
}
packages.sort((a, b) => a.name.localeCompare(b.name));

const out = [];
out.push("# Third-party notices: web UI packages", "");
out.push("The web UI embedded in the `cetra` binary includes the following packages.", "");
let missing = 0;
for (const pkg of packages) {
  out.push(`## ${pkg.name} ${pkg.versions.join(", ")} (${pkg.license})`, "");
  const texts = pkg.paths.flatMap(licenseTexts);
  if (texts.length === 0) {
    missing += 1;
    out.push(
      `No licence file is shipped in the package; its declared licence is ${pkg.license}.`,
      "",
    );
    continue;
  }
  // Every version of a package usually ships the same file; print each distinct text once.
  for (const text of [...new Set(texts)]) {
    out.push("```text", text, "```", "");
  }
}
process.stdout.write(`${out.join("\n")}\n`);
process.stderr.write(
  `js-notices: ${packages.length} packages, ${missing} without a licence file\n`,
);
