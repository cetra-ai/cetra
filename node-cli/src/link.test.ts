import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { CliError } from "./errors.js";
import { findLink, gitTop, type ProjectLink, writeLink } from "./link.js";

const LINK: ProjectLink = {
  server: "http://localhost:8080",
  org: "acme",
  project: "demo",
  project_id: "01900000-0000-7000-8000-000000000001",
};
const temporary: string[] = [];

afterEach(async () => {
  for (const dir of temporary.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

/** A temp directory with a `.git` directory at its top, so the link walk stops there. */
async function repo(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "cetra-link-"));
  temporary.push(dir);
  await mkdir(join(dir, ".git"));
  return dir;
}

describe("project link", () => {
  it("writes the link and finds it from a subdirectory", async () => {
    const top = await repo();
    const sub = join(top, "app", "src");
    await mkdir(sub, { recursive: true });
    const path = await writeLink(join(top, "app"), LINK);
    expect(path).toBe(join(top, "app", ".cetra", "project.json"));
    expect(await findLink(sub)).toEqual(LINK);
  });

  it("writes the file as pretty JSON with a trailing newline", async () => {
    const top = await repo();
    const path = await writeLink(top, LINK);
    expect(await readFile(path, "utf8")).toBe(
      '{\n  "server": "http://localhost:8080",\n  "org": "acme",\n  "project": "demo",\n  "project_id": "01900000-0000-7000-8000-000000000001"\n}\n',
    );
  });

  it("does not look above the top of the git work tree", async () => {
    const outer = await repo();
    await writeLink(outer, LINK);
    const inner = join(outer, "repo");
    await mkdir(join(inner, ".git"), { recursive: true });
    const sub = join(inner, "sub");
    await mkdir(sub, { recursive: true });
    expect(await findLink(sub)).toBeUndefined();
  });

  it("returns undefined when no link exists", async () => {
    const top = await repo();
    expect(await findLink(top)).toBeUndefined();
  });

  it("refuses an invalid link file", async () => {
    const top = await repo();
    await mkdir(join(top, ".cetra"));
    const path = join(top, ".cetra", "project.json");
    for (const text of ["{not json", '{"server":"http://h","org":"a","project":"b"}', "[]"]) {
      await writeFile(path, text);
      const error = await findLink(top).catch((caught: unknown) => caught);
      expect(error).toBeInstanceOf(CliError);
      expect((error as CliError).message).toBe(".cetra/project.json is invalid");
    }
  });
});

describe("gitTop", () => {
  it("finds a .git directory, and a .git file as in a worktree", async () => {
    const top = await repo();
    const sub = join(top, "a", "b");
    await mkdir(sub, { recursive: true });
    expect(await gitTop(sub)).toBe(top);

    const worktree = await mkdtemp(join(tmpdir(), "cetra-link-wt-"));
    temporary.push(worktree);
    await writeFile(join(worktree, ".git"), "gitdir: /elsewhere\n");
    expect(await gitTop(worktree)).toBe(worktree);
  });

  it("finds no git entry in the directory itself when it has none", async () => {
    // An ancestor of the temporary directory may hold a `.git` (the walk goes up to the
    // root), so only the directory itself is asserted.
    const dir = await mkdtemp(join(tmpdir(), "cetra-link-none-"));
    temporary.push(dir);
    const top = await gitTop(dir);
    expect(top === undefined || (top !== dir && dir.startsWith(top))).toBe(true);
  });
});
