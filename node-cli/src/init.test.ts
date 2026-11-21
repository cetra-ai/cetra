import { type ChildProcess, execFile } from "node:child_process";
import { lstat, mkdir, mkdtemp, readFile, rm, stat, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { afterEach, describe, expect, it } from "vitest";
import { credentialsPath, loadCredentials } from "./credentials.js";
import { CliError, type Deps, Exit, type Io } from "./errors.js";
import { init } from "./init.js";
import { type CannedAnswer, type CannedServer, startCanned } from "./test-server.js";
import { VERSION } from "./version.js";

const execute = promisify(execFile);
const SKILL_SOURCE = fileURLToPath(new URL("../../skills/cetra/", import.meta.url));
const PAT = `pat-cetra-${"A".repeat(32)}`;
const KEY = `sk-cetra-${"B".repeat(32)}`;
const OLD_KEY = `sk-cetra-${"C".repeat(32)}`;
const PROJECT_ID = "0190a0b0-0000-7000-8000-000000000002";
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
  project: { id: PROJECT_ID, org_slug: "acme", slug: "demo", name: "Demo" },
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
    expect(link).toEqual({
      server: server.url,
      org: "acme",
      project: "demo",
      project_id: PROJECT_ID,
    });

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

  it("on a moved sign-in saves the token and the link under the cell, and never the typed server", async () => {
    const cell = await canned([{ status: 200, body: GRANT }]);
    const server = await canned([
      { status: 200, body: { status: "ready" } },
      { status: 200, body: DEVICE_CODE },
      { status: 400, body: { code: "authorization_pending" } },
      { status: 400, body: { code: "moved", server: cell.url } },
    ]);
    const { config, dir } = await setup();
    const { io, stdout } = capture({ XDG_CONFIG_HOME: config }, dir);

    const code = await init(
      ["--url", server.url, "--dir", dir, "--yes", "--no-browser", "--no-skill", "--json"],
      io,
      deps,
    );

    expect(code).toBe(Exit.Ok);
    const out = JSON.parse(stdout[0] ?? "") as Record<string, unknown>;
    expect(out).toMatchObject({
      server: GRANT_SERVER,
      traces_url: `${GRANT_SERVER}/acme/demo/traces`,
    });
    expect(await readFile(join(dir, ".env"), "utf8")).toBe(
      `# cetra (npx @cetra-ai/cli init)\nCETRA_BASE_URL=${GRANT_SERVER}\nCETRA_API_KEY=${KEY}\n`,
    );
    const link = JSON.parse(await readFile(join(dir, ".cetra", "project.json"), "utf8"));
    expect(link).toEqual({
      server: cell.url,
      org: "acme",
      project: "demo",
      project_id: PROJECT_ID,
    });
    const saved = await loadCredentials(credentialsPath({ XDG_CONFIG_HOME: config }));
    expect([...saved.keys()]).toEqual([cell.url]);
    expect(saved.get(cell.url)).toBe(PAT);

    expect(server.requests.slice(1).map((request) => request.path)).toEqual([
      "/api/auth/device/code",
      "/api/auth/device/token",
      "/api/auth/device/token",
    ]);
    expect(cell.requests.map((request) => request.path)).toEqual(["/api/auth/device/token"]);
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

  it("keeps an existing key that has no CETRA_BASE_URL, without --yes and without a terminal", async () => {
    const server = await canned(deviceAnswers());
    const { config, dir } = await setup();
    await writeFile(join(dir, ".env"), `CETRA_API_KEY=${OLD_KEY}\n`);
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

  it("follows a symlinked .env: the target is updated and the link stays", async () => {
    const server = await canned(deviceAnswers());
    const { config, dir } = await setup();
    const shared = join(dir, "shared.env");
    await writeFile(shared, "FOO=bar\n");
    await symlink(shared, join(dir, ".env"));
    const { io } = capture({ XDG_CONFIG_HOME: config }, dir);

    const code = await init(
      ["--url", server.url, "--dir", dir, "--yes", "--no-browser", "--no-skill"],
      io,
      deps,
    );

    expect(code).toBe(Exit.Ok);
    expect((await lstat(join(dir, ".env"))).isSymbolicLink()).toBe(true);
    expect(await readFile(shared, "utf8")).toBe(
      `FOO=bar\nCETRA_BASE_URL=${GRANT_SERVER}\nCETRA_API_KEY=${KEY}\n`,
    );
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

/** A `cetra` on the PATH: `cetra version` succeeds, and nothing else is run. */
function cetraOnPath(): typeof execFile {
  return ((
    file: string,
    args: readonly string[],
    _options: unknown,
    callback: (error: Error | null, stdout: string, stderr: string) => void,
  ) => {
    const found = file === "cetra" && args[0] === "version";
    callback(found ? null : Object.assign(new Error("not found"), { code: 1 }), "", "");
  }) as unknown as typeof execFile;
}

/** A free port on 127.0.0.1: a server started and closed again. */
async function freePort(): Promise<number> {
  const probe = await startCanned([]);
  const port = Number(new URL(probe.url).port);
  await probe.close();
  return port;
}

/** Runs `body` with stderr not a terminal, so the admin password is not printed, then restores it. */
async function withStderrNotTerminal<T>(body: () => Promise<T>): Promise<T> {
  const before = Object.getOwnPropertyDescriptor(process.stderr, "isTTY");
  Object.defineProperty(process.stderr, "isTTY", { value: false, configurable: true });
  try {
    return await body();
  } finally {
    if (before === undefined) {
      delete (process.stderr as { isTTY?: boolean }).isTTY;
    } else {
      Object.defineProperty(process.stderr, "isTTY", before);
    }
  }
}

describe.skipIf(process.platform !== "linux")("cetra-cli init on a local first start", () => {
  it("starts the server, signs in with the generated password, and keeps the password off every output", async () => {
    const port = await freePort();
    const url = `http://127.0.0.1:${port}`;
    const dataHome = await tempDir();
    const { config, dir } = await setup();
    const io = capture({ XDG_CONFIG_HOME: config, XDG_DATA_HOME: dataHome }, dir);
    // The server the fake child starts: the readiness check, the sign-in, then the logout.
    const answers: CannedAnswer[] = [
      { status: 200, body: { status: "ready" } },
      {
        status: 200,
        body: { user: {}, orgs: [] },
        headers: { "set-cookie": "cetra_session=SESSION; Path=/; HttpOnly" },
      },
      {
        status: 200,
        body: {
          user: {},
          orgs: [{ id: "o1", slug: "acme", name: "Acme", role: "owner", demo: false }],
        },
      },
      {
        status: 200,
        body: { data: [{ id: PROJECT_ID, slug: "demo", name: "Demo" }], next_cursor: null },
      },
      { status: 201, body: { token: { id: "t1" }, secret: PAT } },
      { status: 201, body: { key: { id: "k1" }, secret: KEY } },
      { status: 204, body: undefined },
    ];
    const spawned: { args: string[]; env: NodeJS.ProcessEnv | undefined }[] = [];
    let served: Promise<CannedServer> | undefined;
    // The fake child is the canned server: it starts listening on the port when it is spawned.
    const fakeSpawn = ((
      _file: string,
      args: readonly string[],
      options: { env?: NodeJS.ProcessEnv },
    ) => {
      spawned.push({ args: [...args], env: options.env });
      served = startCanned(answers, port).then((server) => {
        servers.push(server);
        return server;
      });
      return { pid: 4242, on: () => undefined, unref: () => undefined } as unknown as ChildProcess;
    }) as unknown as NonNullable<Deps["spawn"]>;
    let clock = 0;
    const localDeps = {
      sleep: async () => {},
      now: () => (clock += 1000),
      execFile: cetraOnPath(),
      spawn: fakeSpawn,
      skillSource: SKILL_SOURCE,
    };

    const code = await withStderrNotTerminal(() =>
      init(["--url", url, "--dir", dir, "--yes", "--no-browser", "--no-skill"], io.io, localDeps),
    );
    await served;

    expect(code).toBe(Exit.Ok);
    const password = spawned[0]?.env?.CETRA_INIT_ADMIN_PASSWORD ?? "";
    expect(password).toMatch(/^[A-Za-z0-9]{24}$/);
    expect(spawned[0]?.args).toEqual(["serve", "--config", join(dataHome, "cetra", "local.toml")]);
    expect(spawned[0]?.env?.CETRA_INIT_ADMIN_EMAIL).toBe("admin@cetra.local");

    // The password is in the child's environment only: no output, and no file but the server's own.
    const everything = [...io.stdout, ...io.stderr].join("\n");
    expect(everything).not.toContain(password);
    expect(everything).toContain("Stop the server: kill 4242");
    expect(everything).toContain(
      `Start it again: cetra serve --config ${join(dataHome, "cetra", "local.toml")}`,
    );
    expect(everything).toContain("Admin: admin@cetra.local");
    expect(everything).toContain("reset it with cetra admin reset-password admin@cetra.local");
    expect(await readFile(join(dir, ".env"), "utf8")).toContain(`CETRA_API_KEY=${KEY}\n`);

    // The data dir and its log are owner only.
    const dataDir = join(dataHome, "cetra", "local");
    expect((await stat(dataDir)).mode & 0o777).toBe(0o700);
    expect((await stat(join(dataDir, "server.log"))).mode & 0o777).toBe(0o600);
    expect(await readFile(join(dataHome, "cetra", "local.toml"), "utf8")).toContain(
      `http_addr = "127.0.0.1:${port}"`,
    );
  }, 20_000);

  it("names the reset command when the password sign-in fails after a first start", async () => {
    const port = await freePort();
    const dataHome = await tempDir();
    const { config, dir } = await setup();
    const { io } = capture({ XDG_CONFIG_HOME: config, XDG_DATA_HOME: dataHome }, dir);
    let served: Promise<CannedServer> | undefined;
    const fakeSpawn = (() => {
      served = startCanned(
        [
          { status: 200, body: { status: "ready" } },
          { status: 429, body: { title: "Too Many Requests" } },
        ],
        port,
      ).then((server) => {
        servers.push(server);
        return server;
      });
      return { pid: 4343, on: () => undefined, unref: () => undefined } as unknown as ChildProcess;
    }) as unknown as NonNullable<Deps["spawn"]>;
    let clock = 0;

    const error = await failureOf(
      init(
        ["--url", `http://127.0.0.1:${port}`, "--dir", dir, "--yes", "--no-browser", "--no-skill"],
        io,
        {
          sleep: async () => {},
          now: () => (clock += 1000),
          execFile: cetraOnPath(),
          spawn: fakeSpawn,
          skillSource: SKILL_SOURCE,
        },
      ),
    );
    await served;

    expect(error.code).toBe(Exit.SignIn);
    expect(error.message).toContain("the local server refused the sign-in for now");
    expect(error.message).toContain("reset it with `cetra admin reset-password admin@cetra.local`");
  }, 20_000);
});
