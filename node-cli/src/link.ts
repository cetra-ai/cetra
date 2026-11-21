import { mkdir, readFile, stat, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { CliError, Exit } from "./errors.js";

/** The project a directory is linked to (D-AO-16). It holds no secret, so it can be committed. */
export interface ProjectLink {
  server: string;
  org: string;
  project: string;
  project_id: string;
}

export const LINK_FILE = ".cetra/project.json";

/** The first directory, `dir` itself included, that holds a `.git` entry (a directory or a file,
 *  as in a worktree); undefined when there is none up to the filesystem root. */
export async function gitTop(dir: string): Promise<string | undefined> {
  let current = resolve(dir);
  for (;;) {
    if (await exists(join(current, ".git"))) return current;
    const parent = dirname(current);
    if (parent === current) return undefined;
    current = parent;
  }
}

/** Writes the link under `dir` (creating `.cetra/`) and returns its absolute path. */
export async function writeLink(dir: string, link: ProjectLink): Promise<string> {
  const path = join(resolve(dir), LINK_FILE);
  const text = `${JSON.stringify(
    {
      server: link.server,
      org: link.org,
      project: link.project,
      project_id: link.project_id,
    },
    null,
    2,
  )}\n`;
  try {
    await mkdir(dirname(path), { recursive: true });
    await writeFile(path, text, { encoding: "utf8", mode: 0o644 });
  } catch {
    throw new CliError(`could not write ${LINK_FILE}`, Exit.Write);
  }
  return path;
}

/** The link of `cwd`, else of its nearest parent that has one. The walk stops at the top of the
 *  git work tree that holds `cwd` (or at the filesystem root outside a repository). */
export async function findLink(cwd: string): Promise<ProjectLink | undefined> {
  const top = await gitTop(cwd);
  let current = resolve(cwd);
  for (;;) {
    const link = await readLink(join(current, LINK_FILE));
    if (link !== undefined) return link;
    if (current === top) return undefined;
    const parent = dirname(current);
    if (parent === current) return undefined;
    current = parent;
  }
}

async function readLink(path: string): Promise<ProjectLink | undefined> {
  let text: string;
  try {
    text = await readFile(path, "utf8");
  } catch (error) {
    if (isErrorCode(error, "ENOENT") || isErrorCode(error, "ENOTDIR")) return undefined;
    throw new CliError(`could not read ${LINK_FILE}`, Exit.Error);
  }
  const invalid = () => new CliError(`${LINK_FILE} is invalid`, Exit.Error);
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    throw invalid();
  }
  if (!isRecord(parsed)) throw invalid();
  const { server, org, project, project_id: projectId } = parsed;
  if (
    typeof server !== "string" ||
    typeof org !== "string" ||
    typeof project !== "string" ||
    typeof projectId !== "string"
  ) {
    throw invalid();
  }
  return { server, org, project, project_id: projectId };
}

async function exists(path: string): Promise<boolean> {
  try {
    await stat(path);
    return true;
  } catch {
    return false;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isErrorCode(error: unknown, code: string): boolean {
  return error instanceof Error && "code" in error && error.code === code;
}
