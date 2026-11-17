// Screenshot list check (D-W-12): every shot in screenshots.json has a PNG under
// src/assets/screenshots/, and every PNG there has a shot.
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

export function checkShots(names, files) {
  const findings = [];
  for (const name of names) {
    if (!files.includes(`${name}.png`)) {
      findings.push(`screenshots.json: shot "${name}" has no src/assets/screenshots/${name}.png`);
    }
  }
  for (const file of files) {
    if (file.endsWith(".png") && !names.includes(file.slice(0, -4))) {
      findings.push(`src/assets/screenshots/${file}: no entry in screenshots.json`);
    }
  }
  return findings;
}

export function main(root) {
  const dir = join(root, "src/assets/screenshots");
  const names = JSON.parse(readFileSync(join(root, "screenshots.json"), "utf8")).map((s) => s.name);
  const files = existsSync(dir) ? readdirSync(dir) : [];
  const findings = checkShots(names, files);
  for (const f of findings) console.error(f);
  return findings.length === 0 ? 0 : 1;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  process.exit(main("."));
}
