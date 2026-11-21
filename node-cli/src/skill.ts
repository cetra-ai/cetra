import { randomBytes } from "node:crypto";
import { cp, rename, rm, stat, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { gitTop } from "./link.js";

/** Where the skill goes, relative to the project root (D-AO-9): Claude Code and the Agent Skills
 *  convention. */
export const SKILL_TARGETS = {
  claude: ".claude/skills/cetra",
  agents: ".agents/skills/cetra",
} as const;
/** Written inside an installed skill: its presence says cetra-cli installed the directory. */
export const MARKER = ".cetra-skill";

/** The skill shipped in the package: dist/skills/cetra next to the running bundle (dist/cli.js). */
export function bundledSkill(): string {
  return fileURLToPath(new URL("./skills/cetra/", import.meta.url));
}

/** Copies `source` into each target under `root` and writes the marker (the version and a
 *  newline). A target that exists without the marker is left alone. The copy is built in a
 *  sibling `<target>.tmp-<random>` directory, marker included, and only then put in place, so a
 *  failed or interrupted install keeps the earlier one (and a missing source touches nothing). A
 *  target that fails is skipped with its error message; this function does not throw. */
export async function installSkill(
  source: string,
  root: string,
  targets: (keyof typeof SKILL_TARGETS)[],
  version: string,
): Promise<{ installed: string[]; skipped: { path: string; reason: string }[] }> {
  const installed: string[] = [];
  const skipped: { path: string; reason: string }[] = [];
  for (const target of targets) {
    const path = join(root, SKILL_TARGETS[target]);
    if ((await exists(path)) && !(await exists(join(path, MARKER)))) {
      skipped.push({ path, reason: "not installed by cetra-cli; left alone" });
      continue;
    }
    const temporary = `${path}.tmp-${randomBytes(6).toString("hex")}`;
    try {
      await cp(source, temporary, { recursive: true });
      await writeFile(join(temporary, MARKER), `${version}\n`, "utf8");
      await rm(path, { recursive: true, force: true });
      await rename(temporary, path);
      installed.push(path);
    } catch (error) {
      await rm(temporary, { recursive: true, force: true }).catch(() => undefined);
      skipped.push({ path, reason: error instanceof Error ? error.message : String(error) });
    }
  }
  return { installed, skipped };
}

/** The project root for the skill: the top of the git work tree holding `dir`, else `dir`. */
export async function skillRoot(dir: string): Promise<string> {
  return (await gitTop(dir)) ?? dir;
}

async function exists(path: string): Promise<boolean> {
  try {
    await stat(path);
    return true;
  } catch {
    return false;
  }
}
