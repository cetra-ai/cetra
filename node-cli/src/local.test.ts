import type { ChildProcess, execFile, spawn } from "node:child_process";
import { mkdtemp, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { CliError, Exit, type Io } from "./errors.js";
import {
  configToml,
  DEFAULT_ADMIN_EMAIL,
  generatedPassword,
  isEmail,
  isLoopback,
  isReady,
  localDataDir,
  localServer,
  passwordSignIn,
  startLocal,
} from "./local.js";
import { type CannedAnswer, type CannedServer, startCanned } from "./test-server.js";

const PAT = `pat-cetra-${"A".repeat(32)}`;
const KEY = `sk-cetra-${"B".repeat(32)}`;
const DAY_MS = 24 * 60 * 60 * 1000;

const temporary: string[] = [];
const servers: CannedServer[] = [];

afterEach(async () => {
  vi.restoreAllMocks();
  for (const server of servers.splice(0)) await server.close();
  for (const dir of temporary.splice(0)) await rm(dir, { recursive: true, force: true });
});

async function tempDir(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "cetra-local-"));
  temporary.push(dir);
  return dir;
}

/** An Io that records its lines; the environment is the given one. */
function fakeIo(env: NodeJS.ProcessEnv) {
  const stdout: string[] = [];
  const stderr: string[] = [];
  const io: Io = {
    stdout: (line) => stdout.push(line),
    stderr: (line) => stderr.push(line),
    env,
    cwd: "/",
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

/** A URL on a port nothing listens on: the canned server is started and closed again. */
async function closedUrl(): Promise<URL> {
  const server = await startCanned([]);
  const url = new URL(server.url);
  await server.close();
  return url;
}

/** `execFile` fakes: `answers` is keyed by the program and its first argument, for example
 *  `"docker ps"`. A key that is not listed fails, as a missing program does. */
function fakeExecFile(answers: Record<string, { ok: boolean; stdout?: string; stderr?: string }>) {
  const calls: string[][] = [];
  const fake = (
    file: string,
    args: readonly string[],
    _options: unknown,
    callback: (error: Error | null, stdout: string, stderr: string) => void,
  ) => {
    const key = [file, args[0]].join(" ");
    calls.push([file, ...args]);
    const answer = answers[key] ?? { ok: false };
    const error = answer.ok ? null : Object.assign(new Error(key), { code: 1 });
    callback(error, answer.stdout ?? "", answer.stderr ?? "");
  };
  return { calls, execFile: fake as unknown as typeof execFile };
}

/** A `spawn` fake that records each call and returns a child that never becomes ready. */
function fakeSpawn() {
  const calls: { args: string[]; env: NodeJS.ProcessEnv | undefined }[] = [];
  const fake = (
    _file: string,
    args: readonly string[],
    options: { env?: NodeJS.ProcessEnv },
  ): ChildProcess => {
    calls.push({ args: [...args], env: options.env });
    const child = { pid: 4242, on: () => undefined, unref: () => undefined };
    return child as unknown as ChildProcess;
  };
  return { calls, spawn: fake as unknown as typeof spawn };
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

describe("isLoopback and localServer", () => {
  it("accepts localhost, 127.0.0.0/8 and [::1], and refuses other hosts", () => {
    expect(isLoopback(new URL("http://localhost:8080"))).toBe(true);
    expect(isLoopback(new URL("http://127.0.0.2:8080"))).toBe(true);
    expect(isLoopback(new URL("http://[::1]:8080"))).toBe(true);
    expect(isLoopback(new URL("http://example.com:8080"))).toBe(false);
  });

  it("names the local server as http://localhost:<port>", () => {
    expect(localServer(new URL("http://127.0.0.1:8080"))).toBe("http://localhost:8080");
    expect(localServer(new URL("http://[::1]:9000"))).toBe("http://localhost:9000");
  });

  it("refuses a local URL without a port", () => {
    let thrown: unknown;
    try {
      localServer(new URL("http://localhost"));
    } catch (error) {
      thrown = error;
    }
    expect(thrown).toBeInstanceOf(CliError);
    expect(thrown).toMatchObject({
      message: "--url needs an explicit port for a local server",
      code: Exit.Usage,
    });
  });
});

describe("localDataDir", () => {
  it("follows the platform's data directory on Linux", () => {
    expect(localDataDir({ XDG_DATA_HOME: "/xdg" }, "linux", "/h")).toBe("/xdg/cetra/local");
    expect(localDataDir({ XDG_DATA_HOME: "relative" }, "linux", "/h")).toBe(
      "/h/.local/share/cetra/local",
    );
    expect(localDataDir({}, "linux", "/h")).toBe("/h/.local/share/cetra/local");
  });

  it("uses Application Support on macOS and LOCALAPPDATA on Windows", () => {
    expect(localDataDir({}, "darwin", "/h")).toBe("/h/Library/Application Support/cetra/local");
    expect(localDataDir({ LOCALAPPDATA: "C:\\Users\\a\\AppData\\Local" }, "win32")).toBe(
      "C:\\Users\\a\\AppData\\Local\\cetra\\local",
    );
  });
});

describe("isEmail", () => {
  it("accepts a dotted domain and refuses the rest", () => {
    expect(isEmail(DEFAULT_ADMIN_EMAIL)).toBe(true);
    expect(isEmail("x@y.z")).toBe(true);
    expect(isEmail("admin@localhost")).toBe(false);
    expect(isEmail("a b@c.d")).toBe(false);
    expect(isEmail("a@b.")).toBe(false);
    expect(isEmail("a@.b")).toBe(false);
    expect(isEmail("@c.d")).toBe(false);
    expect(isEmail(`${"a".repeat(65)}@c.d`)).toBe(false);
  });
});

describe("generatedPassword and configToml", () => {
  it("makes a 24-character letters-and-digits password", () => {
    expect(generatedPassword()).toMatch(/^[A-Za-z0-9]{24}$/);
  });

  it("writes the exact config, with a data dir escaped as a TOML string", () => {
    const plain = configToml("/home/a/.local/share/cetra/local", 8080);
    expect(plain).toBe(
      [
        'data_dir = "/home/a/.local/share/cetra/local"',
        'public_url = "http://localhost:8080"',
        'http_addr = "127.0.0.1:8080"',
        'otlp_http_addr = "127.0.0.1:4318"',
        'grpc_addr = "127.0.0.1:4317"',
        "",
      ].join("\n"),
    );

    const quoted = configToml('/x/"y"\\z', 9000);
    expect(quoted.split("\n")[0]).toBe('data_dir = "/x/\\"y\\"\\\\z"');
    expect(quoted).toContain('public_url = "http://localhost:9000"\n');
  });
});

describe("isReady", () => {
  it("is true for a 200 from /readyz and false when nothing listens", async () => {
    const server = await startCanned([{ status: 200, body: { status: "ready" } }]);
    servers.push(server);
    expect(await isReady(server.url)).toBe(true);
    expect(server.requests[0]?.path).toBe("/readyz");

    const closed = await closedUrl();
    expect(await isReady(closed.origin)).toBe(false);
  });
});

describe("passwordSignIn", () => {
  /** The six answers of a full sign-in, in request order. */
  function signInAnswers(): CannedAnswer[] {
    return [
      {
        status: 200,
        body: { user: {}, orgs: [] },
        headers: { "set-cookie": "cetra_session=SESSION; Path=/; HttpOnly" },
      },
      {
        status: 200,
        body: {
          user: {},
          orgs: [
            { id: "o0", slug: "demo-org", name: "Demo", role: "member", demo: true },
            { id: "o1", slug: "acme", name: "Acme", role: "owner", demo: false },
          ],
        },
      },
      {
        status: 200,
        body: { data: [{ id: "p1", slug: "demo", name: "Demo" }], next_cursor: null },
      },
      { status: 201, body: { token: { id: "t1" }, secret: PAT } },
      { status: 201, body: { key: { id: "k1" }, secret: KEY } },
      { status: 204, body: undefined },
    ];
  }

  it("signs in with the origin and cookie and creates the read token and the ingest key", async () => {
    const server = await startCanned(signInAnswers());
    servers.push(server);

    const result = await passwordSignIn(server.url, "admin@cetra.local", "pw", "laptop");

    expect(result).toEqual({ token: PAT, key: KEY, org: "acme", project: "demo", projectId: "p1" });
    const [login, me, projects, token, key, logout] = server.requests;
    expect(login?.path).toBe("/api/auth/login");
    expect(login?.headers.origin).toBe(server.url);
    expect(login?.body).toEqual({ email: "admin@cetra.local", password: "pw" });
    expect(me?.headers.cookie).toBe("cetra_session=SESSION");
    expect(me?.headers.origin).toBe(server.url);
    expect(projects?.path).toBe("/api/v1/orgs/acme/projects?limit=1000");
    expect(token?.path).toBe("/api/v1/me/tokens");
    expect(token?.headers.cookie).toBe("cetra_session=SESSION");
    expect(token?.body).toMatchObject({ name: "CLI init on laptop", scopes: ["read"] });
    const tokenBody = (token?.body ?? {}) as { expires_at?: string };
    const expires = Date.parse(tokenBody.expires_at ?? "");
    expect(Math.abs(expires - (Date.now() + 90 * DAY_MS))).toBeLessThan(60_000);
    expect(key?.path).toBe("/api/v1/projects/acme%2Fdemo/api-keys");
    expect(key?.body).toEqual({ kind: "secret", name: "CLI init on laptop", scopes: ["ingest"] });
    expect(logout?.path).toBe("/api/auth/logout");
    expect(logout?.headers.cookie).toBe("cetra_session=SESSION");
  });

  it("names the token without a host when none is given", async () => {
    const server = await startCanned(signInAnswers());
    servers.push(server);

    await passwordSignIn(server.url, "admin@cetra.local", "pw");

    expect(server.requests[3]?.body).toMatchObject({ name: "CLI init" });
  });

  it("says the local server refused the sign-in when it answers 429", async () => {
    const server = await startCanned([{ status: 429, body: { title: "Too Many Requests" } }]);
    servers.push(server);

    const error = await failureOf(passwordSignIn(server.url, "a@b.c", "pw"));

    expect(error).toMatchObject({
      message: "the local server refused the sign-in for now; try again in a minute",
      code: Exit.SignIn,
    });
  });

  it("refuses a server with no project", async () => {
    const server = await startCanned([
      {
        status: 200,
        body: { user: {}, orgs: [] },
        headers: { "set-cookie": "cetra_session=SESSION; Path=/" },
      },
      {
        status: 200,
        body: {
          user: {},
          orgs: [{ id: "o1", slug: "acme", name: "Acme", role: "owner", demo: false }],
        },
      },
      { status: 200, body: { data: [], next_cursor: null } },
    ]);
    servers.push(server);

    const error = await failureOf(passwordSignIn(server.url, "a@b.c", "pw"));

    expect(error).toMatchObject({ message: "the local server has no project", code: Exit.SignIn });
  });

  it("refuses a server with no organization that is not the demo one", async () => {
    const server = await startCanned([
      {
        status: 200,
        body: { user: {}, orgs: [] },
        headers: { "set-cookie": "cetra_session=SESSION; Path=/" },
      },
      {
        status: 200,
        body: {
          user: {},
          orgs: [{ id: "o0", slug: "demo-org", name: "Demo", role: "member", demo: true }],
        },
      },
    ]);
    servers.push(server);

    const error = await failureOf(passwordSignIn(server.url, "a@b.c", "pw"));

    expect(error).toMatchObject({
      message: "the local server has no organization",
      code: Exit.SignIn,
    });
  });
});

describe.skipIf(process.platform !== "linux")("startLocal", () => {
  it("fails with the neither message and exit 3 when neither cetra nor docker is found", async () => {
    const dataHome = await tempDir();
    const { io } = fakeIo({ XDG_DATA_HOME: dataHome });
    const { execFile: exec } = fakeExecFile({});

    const error = await failureOf(
      startLocal(new URL("http://127.0.0.1:8080"), join(dataHome, "app"), {
        yes: true,
        io,
        deps: { execFile: exec },
      }),
    );

    expect(error.message).toContain("neither cetra nor docker is installed");
    expect(error.code).toBe(Exit.Unreachable);
  });

  it("asks for --yes when there is no terminal and --yes was not given", async () => {
    const dataHome = await tempDir();
    const { io } = fakeIo({ XDG_DATA_HOME: dataHome });
    const { execFile: exec } = fakeExecFile({ "cetra version": { ok: true } });

    const error = await withoutTerminal(() =>
      failureOf(
        startLocal(new URL("http://127.0.0.1:8080"), join(dataHome, "app"), {
          yes: false,
          io,
          deps: { execFile: exec },
        }),
      ),
    );

    expect(error).toMatchObject({
      message: "no cetra server at http://localhost:8080; pass --yes to start one",
      code: Exit.Unreachable,
    });
  });

  it("removes the data dir of a first start whose server never becomes ready", async () => {
    const dataHome = await tempDir();
    const dataDir = join(dataHome, "cetra", "local");
    const dir = join(dataHome, "my-app");
    const { io } = fakeIo({ XDG_DATA_HOME: dataHome });
    const { execFile: exec } = fakeExecFile({ "cetra version": { ok: true } });
    const child = fakeSpawn();
    let clock = 0;
    const url = await closedUrl();
    // The fake child has pid 4242: the kill is a spy, so no real process is signalled.
    const kill = vi.spyOn(process, "kill").mockImplementation(() => true);

    const error = await failureOf(
      startLocal(url, dir, {
        yes: true,
        io,
        deps: {
          execFile: exec,
          spawn: child.spawn,
          sleep: async () => {},
          now: () => (clock += 1000),
        },
      }),
    );

    expect(error.code).toBe(Exit.Unreachable);
    expect(error.message).toMatch(/^cetra did not start; see .*server\.log/);
    expect(child.calls).toHaveLength(1);
    expect(child.calls[0]?.args).toEqual([
      "serve",
      "--config",
      join(dataHome, "cetra", "local.toml"),
    ]);
    const env = child.calls[0]?.env ?? {};
    expect(env.CETRA_INIT_ADMIN_EMAIL).toBe(DEFAULT_ADMIN_EMAIL);
    expect(env.CETRA_INIT_ORG).toBe("local");
    expect(env.CETRA_INIT_PROJECT).toBe("my-app");
    expect(env.CETRA_INIT_ADMIN_PASSWORD).toMatch(/^[A-Za-z0-9]{24}$/);
    expect(kill).toHaveBeenCalledWith(4242);
    await expect(stat(dataDir)).rejects.toThrow();
  });

  it("refuses a container that publishes another port, before it waits", async () => {
    const dataHome = await tempDir();
    const { io } = fakeIo({ XDG_DATA_HOME: dataHome });
    const { execFile: exec } = fakeExecFile({
      "docker version": { ok: true },
      "docker ps": { ok: true, stdout: "cetra-local\n" },
      "docker start": { ok: true },
      "docker port": { ok: true, stdout: "127.0.0.1:9999\n" },
    });
    let sleeps = 0;

    const error = await failureOf(
      startLocal(new URL("http://127.0.0.1:8080"), join(dataHome, "app"), {
        yes: true,
        io,
        deps: {
          execFile: exec,
          sleep: async () => {
            sleeps += 1;
          },
        },
      }),
    );

    expect(error).toMatchObject({
      message:
        "the cetra-local container listens on port 9999; run again with --url http://localhost:9999",
      code: Exit.Usage,
    });
    expect(sleeps).toBe(0);
  });
});
