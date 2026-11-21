import { execFile } from "node:child_process";
import { appendFile, mkdir, open, readFile, rename, rm } from "node:fs/promises";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { promisify } from "node:util";
import { CliError, Exit } from "./errors.js";

const HEADER = "# cetra (npx @cetra-ai/cli init)";
/** The values `init` writes: URLs, keys and plain words, so no quoting is needed. */
const VALUE = /^[\w:/.[\]-]+$/;
const ASSIGNMENT = /^(\s*(?:export\s+)?)([A-Za-z_]\w*)\s*=/;
const run = promisify(execFile);

/** Writes a private (0600) text file: a `<name>.tmp` file, synced, renamed over the target; the
 *  temporary file is removed on failure. Creates missing parent directories, never changes an
 *  existing directory's mode. Used for `.env` and `credentials.toml`. */
export async function writeEnvFile(path: string, text: string): Promise<void> {
  const temporary = `${path}.tmp`;
  try {
    await mkdir(dirname(path), { recursive: true });
    const handle = await open(temporary, "w", 0o600);
    try {
      await handle.writeFile(text, "utf8");
      await handle.sync();
      await handle.chmod(0o600);
    } finally {
      await handle.close();
    }
    await rename(temporary, path);
  } catch {
    await rm(temporary, { force: true }).catch(() => undefined);
    throw new CliError(`could not write ${basename(path)}`, Exit.Write);
  }
}

/** Sets `values` in an env file's text. Lines that assign a name are replaced in place (keeping
 *  `export` and a CR), other lines, comments and order are kept, missing names are appended. A
 *  new file (`text` undefined) starts with a header comment. Keeps a leading BOM and CRLF line
 *  endings. The values are checked first. */
export function setVariables(text: string | undefined, values: Record<string, string>): string {
  for (const [name, value] of Object.entries(values)) {
    if (!VALUE.test(value)) throw new CliError(`invalid value for ${name}`, Exit.Write);
  }
  const source = text ?? "";
  const bom = source.startsWith("\uFEFF") ? "\uFEFF" : "";
  const body = source.slice(bom.length);
  const eolSuffix = body.includes("\r\n") ? "\r" : "";
  const lines = body.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();

  const out: string[] = text === undefined ? [HEADER] : [];
  const found = new Set<string>();
  for (const line of lines) {
    const cr = line.endsWith("\r") ? "\r" : "";
    const match = ASSIGNMENT.exec(line.slice(0, line.length - cr.length));
    const name = match?.[2];
    if (match !== null && name !== undefined && Object.hasOwn(values, name)) {
      found.add(name);
      out.push(`${match[1] ?? ""}${name}=${values[name]}${cr}`);
    } else {
      out.push(line);
    }
  }
  for (const [name, value] of Object.entries(values)) {
    if (!found.has(name)) out.push(`${name}=${value}${eolSuffix}`);
  }
  return out.length === 0 ? "" : `${bom}${out.join("\n")}\n`;
}

/** The value of the last assignment of `name` in an env file's text: quotes removed, spaces and a
 *  CR trimmed. Undefined when the name is not assigned. */
export function readVariable(text: string, name: string): string | undefined {
  let value: string | undefined;
  for (const line of text.replace(/^\uFEFF/, "").split("\n")) {
    const match = /^\s*(?:export\s+)?([A-Za-z_]\w*)\s*=(.*)$/.exec(line.replace(/\r$/, ""));
    if (match?.[1] === name) value = unquote((match[2] ?? "").trim());
  }
  return value;
}

function unquote(value: string): string {
  const first = value.charAt(0);
  if (value.length >= 2 && (first === '"' || first === "'") && value.endsWith(first)) {
    return value.slice(1, -1);
  }
  return value;
}

/** Adds the env file to `<dir>/.gitignore` when `<dir>` is in a git work tree and git does not
 *  already ignore the file. `envPath` is absolute or relative to `dir`. Returns true when it added
 *  the line. Outside a work tree, or with git missing, nothing changes. */
export async function ignoreInGit(dir: string, envPath: string): Promise<boolean> {
  const root = resolve(dir);
  const relativePath = relative(root, resolve(root, envPath));
  if (relativePath === "" || isAbsolute(relativePath) || relativePath.split(sep)[0] === "..") {
    return false;
  }
  if ((await gitExitCode(root, ["rev-parse", "--is-inside-work-tree"])) !== 0) return false;
  const entry = relativePath.split(sep).join("/");
  if ((await gitExitCode(root, ["check-ignore", "-q", "--", entry])) !== 1) return false;

  const gitignore = join(root, ".gitignore");
  let existing = "";
  try {
    existing = await readFile(gitignore, "utf8");
  } catch (error) {
    if (!(error instanceof Error && "code" in error && error.code === "ENOENT")) {
      throw new CliError("could not read .gitignore", Exit.Write);
    }
  }
  const lead = existing !== "" && !existing.endsWith("\n") ? "\n" : "";
  try {
    await appendFile(gitignore, `${lead}${entry}\n`, "utf8");
  } catch {
    throw new CliError("could not write .gitignore", Exit.Write);
  }
  return true;
}

/** The exit status of a git command run in `cwd`; undefined when git cannot run. */
async function gitExitCode(cwd: string, args: string[]): Promise<number | undefined> {
  const env: NodeJS.ProcessEnv = { ...process.env };
  delete env.GIT_DIR;
  delete env.GIT_INDEX_FILE;
  try {
    await run("git", args, { cwd, env });
    return 0;
  } catch (error) {
    return error instanceof Error && "code" in error && typeof error.code === "number"
      ? error.code
      : undefined;
  }
}
