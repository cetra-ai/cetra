import { existsSync } from "node:fs";
import { mkdir, mkdtemp, readdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import { installSkill, MARKER, SKILL_TARGETS, skillRoot } from "./skill.js";

/** The skill in the repository, as the package is built from it. */
const SOURCE = fileURLToPath(new URL("../../skills/cetra/", import.meta.url));
/** The copy the build writes next to the bundle (`dist/skills/cetra`). */
const BUILT = fileURLToPath(new URL("../dist/skills/cetra/", import.meta.url));
const temporary: string[] = [];

afterEach(async () => {
  for (const dir of temporary.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

async function tempDir(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "cetra-skill-"));
  temporary.push(dir);
  return dir;
}

async function exists(path: string): Promise<boolean> {
  try {
    await stat(path);
    return true;
  } catch {
    return false;
  }
}

/** The files under `dir`, relative and sorted; directories are not listed. */
async function files(dir: string): Promise<string[]> {
  const names = await readdir(dir, { recursive: true });
  const result: string[] = [];
  for (const name of names) {
    if ((await stat(join(dir, name))).isFile()) result.push(name);
  }
  return result.sort();
}

describe("installSkill", () => {
  it("writes both targets and the marker", async () => {
    const root = await tempDir();
    const result = await installSkill(SOURCE, root, ["claude", "agents"], "1.2.3");
    expect(result.skipped).toEqual([]);
    expect(result.installed).toEqual([
      join(root, SKILL_TARGETS.claude),
      join(root, SKILL_TARGETS.agents),
    ]);
    for (const target of [SKILL_TARGETS.claude, SKILL_TARGETS.agents]) {
      expect(await exists(join(root, target, "SKILL.md"))).toBe(true);
      expect(await readFile(join(root, target, MARKER), "utf8")).toBe("1.2.3\n");
    }
  });

  it("replaces an earlier install, removing files the new one lacks", async () => {
    const root = await tempDir();
    await installSkill(SOURCE, root, ["claude"], "1.0.0");
    await writeFile(join(root, SKILL_TARGETS.claude, "stale.txt"), "old");
    const result = await installSkill(SOURCE, root, ["claude"], "2.0.0");
    expect(result.installed).toEqual([join(root, SKILL_TARGETS.claude)]);
    expect(await exists(join(root, SKILL_TARGETS.claude, "stale.txt"))).toBe(false);
    expect(await readFile(join(root, SKILL_TARGETS.claude, MARKER), "utf8")).toBe("2.0.0\n");
  });

  it("skips a directory that was not installed by cetra-cli", async () => {
    const root = await tempDir();
    const own = join(root, SKILL_TARGETS.claude);
    await mkdir(own, { recursive: true });
    await writeFile(join(own, "mine.md"), "keep");
    const result = await installSkill(SOURCE, root, ["claude", "agents"], "1.0.0");
    expect(result.installed).toEqual([join(root, SKILL_TARGETS.agents)]);
    expect(result.skipped).toEqual([
      { path: own, reason: "not installed by cetra-cli; left alone" },
    ]);
    expect(await readFile(join(own, "mine.md"), "utf8")).toBe("keep");
  });

  it("writes only the requested targets", async () => {
    const root = await tempDir();
    const result = await installSkill(SOURCE, root, ["claude"], "1.0.0");
    expect(result.installed).toEqual([join(root, SKILL_TARGETS.claude)]);
    expect(await exists(join(root, SKILL_TARGETS.agents))).toBe(false);
  });

  it("skips both targets without throwing when the source does not exist", async () => {
    const root = await tempDir();
    const result = await installSkill(join(root, "missing"), root, ["claude", "agents"], "1.0.0");
    expect(result.installed).toEqual([]);
    expect(result.skipped.map((entry) => entry.path)).toEqual([
      join(root, SKILL_TARGETS.claude),
      join(root, SKILL_TARGETS.agents),
    ]);
    for (const entry of result.skipped) {
      expect(entry.reason).not.toBe("");
    }
  });
});

describe("skillRoot", () => {
  it("returns the top of the git work tree", async () => {
    const top = await tempDir();
    await mkdir(join(top, ".git"));
    const sub = join(top, "app", "src");
    await mkdir(sub, { recursive: true });
    expect(await skillRoot(sub)).toBe(top);
  });
});

describe("the built copy", () => {
  it.skipIf(!existsSync(join(BUILT, "SKILL.md")))("matches the source file for file", async () => {
    const built = await files(BUILT);
    expect(built).toEqual(await files(SOURCE));
    for (const name of built) {
      expect(await readFile(join(BUILT, name), "utf8")).toBe(
        await readFile(join(SOURCE, name), "utf8"),
      );
    }
  });
});
