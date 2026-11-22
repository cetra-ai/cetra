import { chmod, mkdir, readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, posix, win32 } from "node:path";
import { parse, stringify } from "smol-toml";
import { writeEnvFile } from "./envfile.js";
import { CliError, CONTROL, Exit } from "./errors.js";

/** The credentials file of `cetra login`, shared with the Rust CLI (D-AO-17): the same path,
 *  the same `[servers."<url>"]` / `token = "…"` format, mode 0600. */
const FILE_NAME = "credentials.toml";
const BAD_URL = "--url must start with http:// or https://";

/** `credentials.toml`'s path, as the Rust CLI's `dirs::config_dir()` finds it. Linux:
 *  `$XDG_CONFIG_HOME` when it is absolute, else `~/.config`; macOS: `~/Library/Application Support`;
 *  Windows: `%APPDATA%`. `platform` and `home` are parameters so tests can check every platform. */
export function credentialsPath(
  env: NodeJS.ProcessEnv,
  platform: NodeJS.Platform = process.platform,
  home: string = homedir(),
): string {
  if (platform === "win32") {
    const appData = env.APPDATA;
    if (appData === undefined || appData === "") {
      throw new CliError("could not find the config directory", Exit.Error);
    }
    return win32.join(appData, "cetra", FILE_NAME);
  }
  if (platform === "darwin") {
    return posix.join(home, "Library", "Application Support", "cetra", FILE_NAME);
  }
  const xdg = env.XDG_CONFIG_HOME;
  const base = xdg !== undefined && posix.isAbsolute(xdg) ? xdg : posix.join(home, ".config");
  return posix.join(base, "cetra", FILE_NAME);
}

/** Trims and drops every trailing `/`, so one server is one key. */
export function normalizeServerUrl(raw: string): string {
  let url = raw.trim();
  while (url.endsWith("/")) url = url.slice(0, -1);
  return url;
}

/** Whether a server URL is usable: no control character, an `http` or `https` URL, no user or
 *  password. The messages never include the URL. Every path a URL arrives on runs this. */
export function checkServerUrl(url: string): void {
  // The URL parser strips control characters, so they are refused before parsing.
  if (url.search(CONTROL) !== -1) throw new CliError(BAD_URL, Exit.Usage);
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    throw new CliError(BAD_URL, Exit.Usage);
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new CliError(BAD_URL, Exit.Usage);
  }
  if (parsed.username !== "" || parsed.password !== "") {
    throw new CliError("the server URL must not contain credentials", Exit.Usage);
  }
}

/** The stored form of a raw server URL: trim, check, then normalize (the Rust order). */
export function serverUrl(raw: string): string {
  const trimmed = raw.trim();
  checkServerUrl(trimmed);
  return normalizeServerUrl(trimmed);
}

/** The server URL when no `--url` is given: the only saved server. Several saved servers, or none,
 *  need `--url`. */
export function chosenServer(saved: Map<string, string>): string {
  if (saved.size === 0) {
    throw new CliError("no saved server; run `cetra-cli login --url <URL>`", Exit.Usage);
  }
  if (saved.size > 1) throw new CliError("more than one saved server; pass --url", Exit.Usage);
  const [server] = [...saved.keys()];
  if (server === undefined) {
    throw new CliError("no saved server; run `cetra-cli login --url <URL>`", Exit.Usage);
  }
  checkServerUrl(server);
  return server;
}

/** The saved tokens, server URL → token. A missing file is empty; a file that does not parse, or
 *  whose `servers` is not a table of `{ token }` tables, is an error. */
export async function loadCredentials(path: string): Promise<Map<string, string>> {
  let text: string;
  try {
    text = await readFile(path, "utf8");
  } catch (error) {
    if (isErrorCode(error, "ENOENT")) return new Map();
    throw new CliError(`could not read ${FILE_NAME}`, Exit.Error);
  }
  const invalid = () => new CliError("credentials file is invalid", Exit.Error);
  let parsed: unknown;
  try {
    parsed = parse(text);
  } catch {
    throw invalid();
  }
  if (!isRecord(parsed)) throw invalid();
  const servers = parsed.servers ?? {};
  if (!isRecord(servers)) throw invalid();
  const tokens = new Map<string, string>();
  for (const [server, entry] of Object.entries(servers)) {
    if (!isRecord(entry) || typeof entry.token !== "string") throw invalid();
    tokens.set(server, entry.token);
  }
  return tokens;
}

/** Saves `token` for `server` and keeps the other servers. The directory becomes 0700 (its mode
 *  is changed on every save, as in Rust), the file 0600, and the servers are written sorted. */
export async function saveToken(path: string, server: string, token: string): Promise<void> {
  const tokens = await loadCredentials(path);
  tokens.set(server, token);
  const servers: Record<string, { token: string }> = {};
  for (const url of [...tokens.keys()].sort()) {
    servers[url] = { token: tokens.get(url) ?? "" };
  }
  const directory = dirname(path);
  try {
    await mkdir(directory, { recursive: true });
    if (process.platform !== "win32") await chmod(directory, 0o700);
  } catch {
    throw new CliError("could not create the credentials directory", Exit.Write);
  }
  await writeEnvFile(path, stringify({ servers }));
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isErrorCode(error: unknown, code: string): boolean {
  return isRecord(error) && error.code === code;
}
