import { execFile } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { afterEach, describe, expect, it } from "vitest";
import { credentialsPath, loadCredentials } from "./credentials.js";
import { CliError, Exit, type Io } from "./errors.js";
import { init } from "./init.js";
import { type CannedAnswer, type CannedServer, startCanned } from "./test-server.js";
import { VERSION } from "./version.js";

const execute = promisify(execFile);
const SKILL_SOURCE = fileURLToPath(new URL("../../skills/cetra/", import.meta.url));
const PAT = `pat-cetra-${"A".repeat(32)}`;
const KEY = `sk-cetra-${"B".repeat(32)}`;
const OLD_KEY = `sk-cetra-${"C".repeat(32)}`;
// The grant names the public URL; the canned server is the one init talks to.
const GRANT_SERVER = "https://eu1.cetra.test";
const DEVICE_CODE = {
  device_code: "device-code-1",
  user_code: "ABCD-EFGH",
  verification_uri_complete: `${GRANT_SERVER}/device`,
  expires_in: 900,
  interval: 1,
};
const GRANT = {
  server: GRANT_SERVER,
  token: PAT,
  project: { id: "p1", org_slug: "acme", slug: "demo", name: "Demo" },
  ingest_key: KEY,
};
const deps = { sleep: async () => {}, skillSource: SKILL_SOURCE };

const temporary: string[] = [];
const servers: CannedServer[] = [];

afterEach(async () => {
  for (const server of servers.splice(0)) await server.close();
  for (const dir of temporary.splice(0)) await rm(dir, { recursive: true, force: true });
});

async function tempDir(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "cetra-init-"));
  temporary.push(dir);
  return dir;
}

/** A config directory and a project directory, both temporary. The project is its own git work
 *  tree, so the skill root is the project directory: the temporary directory may sit inside another
 *  repository (the test directory is under /tmp, which can be one). */
async function setup(): Promise<{ config: string; dir: string }> {
  const dir = await tempDir();
  await execute("git", ["init", "-q"], { cwd: dir, env: gitEnvironment() });
  return { config: await tempDir(), dir };
}

async function canned(answers: CannedAnswer[]): Promise<CannedServer> {
  const server = await startCanned(answers);
  servers.push(server);
  return server;
}

/** An Io that records its lines. The environment is exactly the one given. */
function capture(env: NodeJS.ProcessEnv, cwd: string) {
  const stdout: string[] = [];
  const stderr: string[] = [];
  const io: Io = {
    stdout: (line) => stdout.push(line),
    stderr: (line) => stderr.push(line),
    env,
    cwd,
  };
  return { io, stdout, stderr };
}

async function failureOf(promise: Promise<unknown>): Promise<CliError> {
  try {
    await promise;
  } catch (error) {
    if (error instanceof CliError) return error;
    throw error;
  }
  throw new Error("expected the command to fail");
}

/** Runs `body` with stdin not a terminal, then restores it. */
async function withoutTerminal<T>(body: () => Promise<T>): Promise<T> {
  const before = Object.getOwnPropertyDescriptor(process.stdin, "isTTY");
  Object.defineProperty(process.stdin, "isTTY", { value: false, configurable: true });
  try {
    return await body();
  } finally {
    if (before === undefined) {
      delete (process.stdin as { isTTY?: boolean }).isTTY;
    } else {
      Object.defineProperty(process.stdin, "isTTY", before);
    }
  }
}

/** The answers of a device sign-in that waits once and then gets the grant. */
function deviceAnswers(): CannedAnswer[] {
  return [
    { status: 200, body: { status: "ready" } },
    { status: 200, body: DEVICE_CODE },
    { status: 400, body: { code: "authorization_pending" } },
    { status: 200, body: GRANT },
  ];
}

/** Git without the variables that would point it at another repository. */
function gitEnvironment(): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env };
  delete env.GIT_DIR;
  delete env.GIT_INDEX_FILE;
  return env;
}

// The saved credentials live under XDG_CONFIG_HOME, which these tests set; that path is only
// honoured on Linux, so the tests do not run elsewhere and never touch a real config directory.
describe.skipIf(process.platform !== "linux")("cetra-cli init", () => {
  it("on the device path writes .env, the link, the credentials and both skills", async () => {
    const server = await canned(deviceAnswers());
    const { config, dir } = await setup();
    const { io, stdout, stderr } = capture({ XDG_CONFIG_HOME: config }, dir);

    const code = await init(
      ["--url", server.url, "--dir", dir, "--yes", "--no-browser", "--json"],
      io,
      deps,
    );

    expect(code).toBe(Exit.Ok);
    expect(stdout).toHaveLength(1);
    const out = JSON.parse(stdout[0] ?? "") as Record<string, unknown>;
    expect(out).toEqual({
      server: GRANT_SERVER,
      project: "acme/demo",
      env_file: join(dir, ".env"),
      link_file: join(dir, ".cetra", "project.json"),
      skills: [join(dir, ".claude", "skills", "cetra"), join(dir, ".agents", "skills", "cetra")],
      prompt: expect.stringContaining("Add cetra tracing to this app.") as unknown,
      traces_url: `${GRANT_SERVER}/acme/demo/traces`,
    });

    const envPath = join(dir, ".env");
    expect(await readFile(envPath, "utf8")).toBe(
      `# cetra (npx @cetra-ai/cli init)\nCETRA_BASE_URL=${GRANT_SERVER}\nCETRA_API_KEY=${KEY}\n`,
    );
    expect((await stat(envPath)).mode & 0o777).toBe(0o600);

    const link = JSON.parse(await readFile(join(dir, ".cetra", "project.json"), "utf8"));
    expect(link).toEqual({ server: server.url, org: "acme", project: "demo", project_id: "p1" });

    const saved = await loadCredentials(credentialsPath({ XDG_CONFIG_HOME: config }));
    expect(saved.get(server.url)).toBe(PAT);

    for (const target of [".claude/skills/cetra", ".agents/skills/cetra"]) {
      const installed = join(dir, target);
      expect(await readFile(join(installed, ".cetra-skill"), "utf8")).toBe(`${VERSION}\n`);
      await expect(stat(join(installed, "SKILL.md"))).resolves.toBeDefined();
    }

    const secrets = [...stdout, ...stderr].join("\n");
    expect(secrets).not.toContain(PAT);
    expect(secrets).not.toContain(KEY);

    const codeRequest = server.requests[1];
    expect(codeRequest?.path).toBe("/api/auth/device/code");
    expect(codeRequest?.body).toMatchObject({ purpose: "init" });
    expect(codeRequest?.headers.origin).toBeUndefined();
    expect(server.requests[2]?.path).toBe("/api/auth/device/token");
  });

  it("replaces an existing key for the same server with --yes and keeps the other lines", async () => {
    const server = await canned(deviceAnswers());
    const { config, dir } = await setup();
    await writeFile(
      join(dir, ".env"),
      `# keep me\nFOO=bar\nexport CETRA_API_KEY=${OLD_KEY}\nCETRA_BASE_URL=${GRANT_SERVER}\n`,
    );
    const { io } = capture({ XDG_CONFIG_HOME: config }, dir);

    const code = await init(
      ["--url", server.url, "--dir", dir, "--yes", "--no-browser", "--no-skill"],
      io,
      deps,
    );

    expect(code).toBe(Exit.Ok);
    expect(await readFile(join(dir, ".env"), "utf8")).toBe(
      `# keep me\nFOO=bar\nexport CETRA_API_KEY=${KEY}\nCETRA_BASE_URL=${GRANT_SERVER}\n`,
    );
  });

  it("keeps an existing key without --yes and without a terminal, and says so", async () => {
    const server = await canned(deviceAnswers());
    const { config, dir } = await setup();
    await writeFile(
      join(dir, ".env"),
      `CETRA_API_KEY=${OLD_KEY}\nCETRA_BASE_URL=${GRANT_SERVER}\n`,
    );
    const { io, stderr } = capture({ XDG_CONFIG_HOME: config }, dir);

    const code = await withoutTerminal(() =>
      init(["--url", server.url, "--dir", dir, "--no-browser", "--no-skill"], io, deps),
    );

    expect(code).toBe(Exit.Ok);
    expect(await readFile(join(dir, ".env"), "utf8")).toBe(
      `CETRA_API_KEY=${OLD_KEY}\nCETRA_BASE_URL=${GRANT_SERVER}\n`,
    );
    expect(stderr).toContain("kept the existing CETRA_API_KEY in .env");
  });

  it("adds the env file to .gitignore in a git work tree", async () => {
    const server = await canned(deviceAnswers());
    const { config, dir } = await setup();
    const { io, stderr } = capture({ XDG_CONFIG_HOME: config }, dir);

    const code = await init(
      ["--url", server.url, "--dir", dir, "--yes", "--no-browser", "--no-skill"],
      io,
      deps,
    );

    expect(code).toBe(Exit.Ok);
    expect(await readFile(join(dir, ".gitignore"), "utf8")).toBe(".env\n");
    expect(stderr).toContain("Added .env to .gitignore.");
  });

  it("gives exit 3 for a non-loopback server that does not answer", async () => {
    const { config, dir } = await setup();
    const { io } = capture({ XDG_CONFIG_HOME: config }, dir);

    const error = await failureOf(
      init(["--url", "http://192.0.2.1:9", "--dir", dir, "--no-browser"], io, deps),
    );

    expect(error).toMatchObject({
      message: "no cetra server at http://192.0.2.1:9",
      code: Exit.Unreachable,
    });
  }, 15_000);

  it("gives exit 4 when the sign-in is denied", async () => {
    const server = await canned([
      { status: 200, body: { status: "ready" } },
      { status: 200, body: DEVICE_CODE },
      { status: 400, body: { code: "access_denied" } },
    ]);
    const { config, dir } = await setup();
    const { io } = capture({ XDG_CONFIG_HOME: config }, dir);

    const error = await failureOf(
      init(["--url", server.url, "--dir", dir, "--no-browser", "--no-skill"], io, deps),
    );

    expect(error).toMatchObject({ message: "sign-in denied in the browser", code: Exit.SignIn });
  });

  it("gives exit 5 when the env file path is a directory", async () => {
    const server = await canned([
      { status: 200, body: { status: "ready" } },
      { status: 200, body: DEVICE_CODE },
      { status: 200, body: GRANT },
    ]);
    const { config, dir } = await setup();
    await mkdir(join(dir, ".env"));
    const { io } = capture({ XDG_CONFIG_HOME: config }, dir);

    const error = await failureOf(
      init(["--url", server.url, "--dir", dir, "--no-browser", "--no-skill"], io, deps),
    );

    expect(error).toMatchObject({ message: "could not write .env", code: Exit.Write });
  });

  it("gives exit 2 for --agents with --no-skill, an unknown agent, and a missing --dir", async () => {
    const { config, dir } = await setup();
    const { io } = capture({ XDG_CONFIG_HOME: config }, dir);

    const combined = await failureOf(
      init(["--agents", "claude", "--no-skill", "--dir", dir], io, deps),
    );
    expect(combined.code).toBe(Exit.Usage);

    const unknown = await failureOf(init(["--agents", "frob", "--dir", dir], io, deps));
    expect(unknown.code).toBe(Exit.Usage);

    const missing = await failureOf(init(["--dir", resolve(dir, "no-such-directory")], io, deps));
    expect(missing.code).toBe(Exit.Usage);
  });
});
