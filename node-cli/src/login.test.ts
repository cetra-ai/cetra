import { mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { CliError, Exit, type Io } from "./errors.js";
import { login } from "./login.js";
import { type CannedServer, startCanned } from "./test-server.js";

const PAT = `pat-cetra-${"A".repeat(32)}`;
const CODE = {
  device_code: "d".repeat(43),
  user_code: "BCDF-GHJK",
  verification_uri: "http://127.0.0.1:9/device",
  verification_uri_complete: "http://127.0.0.1:9/device?code=BCDF-GHJK",
  expires_in: 600,
  interval: 0,
};

const temporary: string[] = [];
const servers: CannedServer[] = [];

afterEach(async () => {
  for (const server of servers.splice(0)) await server.close();
  for (const dir of temporary.splice(0)) await rm(dir, { recursive: true, force: true });
});

async function tempDir(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "cetra-login-"));
  temporary.push(dir);
  return dir;
}

/** An Io that records its lines; the config directory is the given temporary one. */
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

const fast = { sleep: async () => {}, now: () => 0 };

async function failureOf(promise: Promise<unknown>): Promise<CliError> {
  try {
    await promise;
  } catch (error) {
    if (error instanceof CliError) return error;
    throw error;
  }
  throw new Error("expected the command to fail");
}

// The credentials file lives under XDG_CONFIG_HOME, which these tests set; that path is only
// honoured on Linux, so the tests do not run elsewhere and never touch a real config directory.
describe.skipIf(process.platform !== "linux")("cetra-cli login", () => {
  it("signs in, checks the token and saves it; stdout stays empty without --json", async () => {
    const config = await tempDir();
    const server = await startCanned([
      { status: 200, body: CODE },
      { status: 200, body: { server: "http://127.0.0.1:8080", token: PAT } },
      { status: 200, body: { user: {} } },
    ]);
    servers.push(server);
    const { io, stdout, stderr } = fakeIo({ XDG_CONFIG_HOME: config });

    const code = await login(["--url", server.url, "--no-browser"], io, fast);

    expect(code).toBe(Exit.Ok);
    expect(stdout).toEqual([]);
    expect(stderr).toContain("Code: BCDF-GHJK");
    expect(stderr).toContain(`Signed in to ${server.url}. The token expires in 90 days.`);
    const me = server.requests[2];
    expect(me?.path).toBe("/api/auth/me");
    expect(me?.headers.authorization).toBe(`Bearer ${PAT}`);
    const saved = await readFile(join(config, "cetra", "credentials.toml"), "utf8");
    expect(saved).toBe(`[servers."${server.url}"]\ntoken = "${PAT}"\n`);
  });

  it("prints the server as JSON on stdout with --json", async () => {
    const config = await tempDir();
    const server = await startCanned([
      { status: 200, body: CODE },
      { status: 200, body: { server: "http://127.0.0.1:8080", token: PAT } },
      { status: 200, body: { user: {} } },
    ]);
    servers.push(server);
    const { io, stdout } = fakeIo({ XDG_CONFIG_HOME: config });

    await login(["--url", server.url, "--no-browser", "--json"], io, fast);

    expect(stdout).toEqual([JSON.stringify({ server: server.url })]);
  });

  it("refuses a token the server does not accept and saves nothing", async () => {
    const config = await tempDir();
    const server = await startCanned([
      { status: 200, body: CODE },
      { status: 200, body: { server: "http://127.0.0.1:8080", token: PAT } },
      { status: 401, body: { title: "Unauthorized" } },
    ]);
    servers.push(server);
    const { io } = fakeIo({ XDG_CONFIG_HOME: config });

    const error = await failureOf(login(["--url", server.url, "--no-browser"], io, fast));

    expect(error).toMatchObject({ message: "the server rejected the token", code: Exit.SignIn });
    await expect(stat(join(config, "cetra", "credentials.toml"))).rejects.toThrow();
  });

  it("refuses a --url that is not http or https", async () => {
    const { io } = fakeIo({ XDG_CONFIG_HOME: await tempDir() });
    const error = await failureOf(login(["--url", "ftp://example.com", "--no-browser"], io, fast));
    expect(error).toMatchObject({
      message: "--url must start with http:// or https://",
      code: Exit.Usage,
    });
  });

  it("needs --url, CETRA_URL or one saved server", async () => {
    const { io } = fakeIo({ XDG_CONFIG_HOME: await tempDir() });
    const error = await failureOf(login(["--no-browser"], io, fast));
    expect(error).toMatchObject({
      message: "no saved server; run `cetra-cli login --url <URL>`",
      code: Exit.Usage,
    });
  });

  it("takes the server from CETRA_URL when --url is not given", async () => {
    const config = await tempDir();
    const server = await startCanned([
      { status: 200, body: CODE },
      { status: 200, body: { server: "http://127.0.0.1:8080", token: PAT } },
      { status: 200, body: { user: {} } },
    ]);
    servers.push(server);
    const { io } = fakeIo({ XDG_CONFIG_HOME: config, CETRA_URL: `${server.url}/` });

    await login(["--no-browser"], io, fast);

    const saved = await readFile(join(config, "cetra", "credentials.toml"), "utf8");
    expect(saved).toBe(`[servers."${server.url}"]\ntoken = "${PAT}"\n`);
  });
});
