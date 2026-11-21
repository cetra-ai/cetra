import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { credentialsPath, saveToken } from "./credentials.js";
import { CliError, Exit, type Io } from "./errors.js";
import { writeLink } from "./link.js";
import { query } from "./query.js";
import { type CannedServer, startCanned } from "./test-server.js";

const PAT = `pat-cetra-${"B".repeat(32)}`;
const ENV_TOKEN = `pat-cetra-${"C".repeat(32)}`;
const PROJECT_ID = "01900000-0000-7000-8000-000000000001";
const PROJECT_PATH = "/api/v1/projects/acme%2Fdemo/sql";
const NOW = Date.parse("2026-01-02T00:00:00.000Z");
const deps = { now: () => NOW };
const ONE_ROW = { columns: [{ name: "n", type: "Int64" }], rows: [{ n: 1 }], truncated: false };
const PROBLEM_400 = {
  type: "about:blank",
  title: "Bad Request",
  status: 400,
  code: "invalid_filter",
  detail: "SQL parse error",
  errors: [{ field: "query", message: "unexpected token", position: { line: 1, column: 8 } }],
};

const temporary: string[] = [];
const servers: CannedServer[] = [];

afterEach(async () => {
  for (const server of servers.splice(0)) await server.close();
  for (const dir of temporary.splice(0)) await rm(dir, { recursive: true, force: true });
});

async function tempDir(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "cetra-query-"));
  temporary.push(dir);
  return dir;
}

/** The config directory and a working directory with no link, both temporary. */
async function dirs(): Promise<{ config: string; cwd: string }> {
  return { config: await tempDir(), cwd: await tempDir() };
}

/** An Io that records its lines. The environment is exactly the one given. */
function fakeIo(env: NodeJS.ProcessEnv, cwd: string) {
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

async function canned(answers: { status: number; body: unknown }[]): Promise<CannedServer> {
  const server = await startCanned(answers);
  servers.push(server);
  return server;
}

// The saved credentials live under XDG_CONFIG_HOME, which these tests set; that path is only
// honoured on Linux, so the tests do not run elsewhere and never touch a real config directory.
describe.skipIf(process.platform !== "linux")("cetra-cli query", () => {
  it("posts the statement, the default 24h window and the limit with the token", async () => {
    const server = await canned([{ status: 200, body: ONE_ROW }]);
    const { config, cwd } = await dirs();
    const { io, stdout, stderr } = fakeIo({ XDG_CONFIG_HOME: config, CETRA_TOKEN: PAT }, cwd);

    const code = await query(
      ["SELECT 1 AS n", "--url", server.url, "--project", "acme/demo"],
      io,
      deps,
    );

    expect(code).toBe(Exit.Ok);
    expect(stdout).toEqual(["n", "-", "1"]);
    expect(stderr).toEqual(["1 row"]);
    const [request] = server.requests;
    expect(request?.method).toBe("POST");
    expect(request?.path).toBe(PROJECT_PATH);
    expect(request?.headers.authorization).toBe(`Bearer ${PAT}`);
    expect(request?.body).toEqual({
      query: "SELECT 1 AS n",
      from: "2026-01-01T00:00:00.000000000Z",
      to: "2026-01-02T00:00:00.000000000Z",
      limit: 100,
    });
  });

  it("sends --from and --to as given, without the default window", async () => {
    const server = await canned([{ status: 200, body: ONE_ROW }]);
    const { config, cwd } = await dirs();
    const { io } = fakeIo({ XDG_CONFIG_HOME: config, CETRA_TOKEN: PAT }, cwd);

    await query(
      [
        "SELECT 1",
        "--url",
        server.url,
        "--project",
        "acme/demo",
        "--from",
        "2025-12-31T00:00:00Z",
        "--to",
        "2026-01-02T00:00:00Z",
      ],
      io,
      deps,
    );

    expect(server.requests[0]?.body).toMatchObject({
      from: "2025-12-31T00:00:00Z",
      to: "2026-01-02T00:00:00Z",
    });
  });

  it("prints the columns and rows as JSON on stdout with --json, and no note", async () => {
    const server = await canned([{ status: 200, body: ONE_ROW }]);
    const { config, cwd } = await dirs();
    const { io, stdout, stderr } = fakeIo({ XDG_CONFIG_HOME: config, CETRA_TOKEN: PAT }, cwd);

    await query(["SELECT 1", "--url", server.url, "--project", "acme/demo", "--json"], io, deps);

    expect(stdout).toEqual([
      JSON.stringify({ columns: ONE_ROW.columns, rows: [{ n: 1 }], truncated: false }),
    ]);
    expect(stderr).toEqual([]);
  });

  it("counts rows on stderr: 0 rows and many rows", async () => {
    const server = await canned([
      {
        status: 200,
        body: { columns: [{ name: "n", type: "Int64" }], rows: [], truncated: false },
      },
      {
        status: 200,
        body: {
          columns: [{ name: "n", type: "Int64" }],
          rows: [{ n: 1 }, { n: 2 }],
          truncated: false,
        },
      },
    ]);
    const { config, cwd } = await dirs();
    const { io, stdout, stderr } = fakeIo({ XDG_CONFIG_HOME: config, CETRA_TOKEN: PAT }, cwd);
    const args = ["SELECT 1", "--url", server.url, "--project", "acme/demo"];

    await query(args, io, deps);
    expect(stdout).toEqual(["n", "-"]);
    expect(stderr).toEqual(["0 rows"]);

    stdout.length = 0;
    stderr.length = 0;
    await query(args, io, deps);
    expect(stdout).toEqual(["n", "-", "1", "2"]);
    expect(stderr).toEqual(["2 rows"]);
  });

  it("notes a truncated answer and sends the given limit", async () => {
    const server = await canned([
      {
        status: 200,
        body: { columns: [{ name: "n", type: "Int64" }], rows: [{ n: 1 }], truncated: true },
      },
    ]);
    const { config, cwd } = await dirs();
    const { io, stderr } = fakeIo({ XDG_CONFIG_HOME: config, CETRA_TOKEN: PAT }, cwd);

    await query(
      ["SELECT 1", "--url", server.url, "--project", "acme/demo", "--limit", "1"],
      io,
      deps,
    );

    expect(stderr).toEqual(["1 row; more rows matched, raise --limit"]);
    expect(server.requests[0]?.body).toMatchObject({ limit: 1 });
  });

  it("gives Exit.Query and the problem message for a 400 with a position", async () => {
    const server = await canned([{ status: 400, body: PROBLEM_400 }]);
    const { config, cwd } = await dirs();
    const { io, stdout } = fakeIo({ XDG_CONFIG_HOME: config, CETRA_TOKEN: PAT }, cwd);

    const error = await failureOf(
      query(["SELEC 1", "--url", server.url, "--project", "acme/demo"], io, deps),
    );

    expect(error).toMatchObject({
      message:
        "Bad Request (invalid_filter): SQL parse error\n  query: unexpected token (line 1, column 8)",
      code: Exit.Query,
    });
    expect(stdout).toEqual([]);
  });

  it("gives Exit.SignIn for a 401 and names the login command", async () => {
    const server = await canned([{ status: 401, body: { title: "Unauthorized" } }]);
    const { config, cwd } = await dirs();
    const { io } = fakeIo({ XDG_CONFIG_HOME: config, CETRA_TOKEN: PAT }, cwd);

    const error = await failureOf(
      query(["SELECT 1", "--url", server.url, "--project", "acme/demo"], io, deps),
    );

    expect(error).toMatchObject({
      message: `the server rejected the token; run \`cetra-cli login --url ${server.url}\``,
      code: Exit.SignIn,
    });
  });

  it("names the status of a non-problem error answer", async () => {
    const server = await canned([{ status: 502, body: "oops" }]);
    const { config, cwd } = await dirs();
    const { io } = fakeIo({ XDG_CONFIG_HOME: config, CETRA_TOKEN: PAT }, cwd);

    const error = await failureOf(
      query(["SELECT 1", "--url", server.url, "--project", "acme/demo"], io, deps),
    );

    expect(error).toMatchObject({ message: "the server answered 502", code: Exit.Query });
  });

  it("refuses a 2xx answer that is not the result shape", async () => {
    const server = await canned([{ status: 200, body: { rows: [] } }]);
    const { config, cwd } = await dirs();
    const { io } = fakeIo({ XDG_CONFIG_HOME: config, CETRA_TOKEN: PAT }, cwd);

    const error = await failureOf(
      query(["SELECT 1", "--url", server.url, "--project", "acme/demo"], io, deps),
    );

    expect(error.message).toBe("the server sent an invalid response");
  });

  it("resolves the server and project from the link, and the token from the credentials", async () => {
    const server = await canned([{ status: 200, body: ONE_ROW }]);
    const { config, cwd } = await dirs();
    await writeLink(cwd, {
      server: server.url,
      org: "acme",
      project: "demo",
      project_id: PROJECT_ID,
    });
    await saveToken(credentialsPath({ XDG_CONFIG_HOME: config }), server.url, PAT);
    const { io, stdout } = fakeIo({ XDG_CONFIG_HOME: config }, cwd);

    await query(["SELECT 1 AS n"], io, deps);

    expect(stdout).toEqual(["n", "-", "1"]);
    expect(server.requests[0]?.path).toBe(PROJECT_PATH);
    expect(server.requests[0]?.headers.authorization).toBe(`Bearer ${PAT}`);
  });

  it("never sends CETRA_TOKEN to a server that comes from the link file", async () => {
    const server = await canned([{ status: 200, body: ONE_ROW }]);
    const { config, cwd } = await dirs();
    await writeLink(cwd, {
      server: server.url,
      org: "acme",
      project: "demo",
      project_id: PROJECT_ID,
    });
    const { io } = fakeIo({ XDG_CONFIG_HOME: config, CETRA_TOKEN: ENV_TOKEN }, cwd);

    const error = await failureOf(query(["SELECT 1"], io, deps));

    expect(error).toMatchObject({
      message: `no saved token for ${server.url}; run \`cetra-cli login --url ${server.url}\``,
      code: Exit.SignIn,
    });
    expect(server.requests).toEqual([]);
  });

  it("takes the server, project and token from CETRA_URL, CETRA_PROJECT and CETRA_TOKEN", async () => {
    const server = await canned([{ status: 200, body: ONE_ROW }]);
    const { config, cwd } = await dirs();
    const { io, stdout } = fakeIo(
      {
        XDG_CONFIG_HOME: config,
        CETRA_URL: `${server.url}/`,
        CETRA_PROJECT: "acme/demo",
        CETRA_TOKEN: PAT,
      },
      cwd,
    );

    await query(["SELECT 1 AS n"], io, deps);

    expect(stdout).toEqual(["n", "-", "1"]);
    expect(server.requests[0]?.path).toBe(PROJECT_PATH);
    expect(server.requests[0]?.headers.authorization).toBe(`Bearer ${PAT}`);
  });

  it("refuses a link whose server is not a valid URL", async () => {
    const { config, cwd } = await dirs();
    await writeLink(cwd, {
      server: "ftp://example.com",
      org: "acme",
      project: "demo",
      project_id: PROJECT_ID,
    });
    const { io } = fakeIo({ XDG_CONFIG_HOME: config }, cwd);

    const error = await failureOf(query(["SELECT 1"], io, deps));

    expect(error).toMatchObject({ message: ".cetra/project.json is invalid", code: Exit.Error });
  });

  it("names what is missing: the server, the project, the token", async () => {
    const { config, cwd } = await dirs();
    const env = { XDG_CONFIG_HOME: config };

    const noServer = await failureOf(query(["SELECT 1"], fakeIo(env, cwd).io, deps));
    expect(noServer).toMatchObject({
      message: "no server: run `cetra-cli init`, or pass --url",
      code: Exit.Usage,
    });

    const noProject = await failureOf(
      query(["SELECT 1", "--url", "http://127.0.0.1:9"], fakeIo(env, cwd).io, deps),
    );
    expect(noProject).toMatchObject({
      message: "no project: run `cetra-cli init`, or pass --project",
      code: Exit.Usage,
    });

    const noToken = await failureOf(
      query(
        ["SELECT 1", "--url", "http://127.0.0.1:9", "--project", "acme/demo"],
        fakeIo(env, cwd).io,
        deps,
      ),
    );
    expect(noToken).toMatchObject({
      message:
        "no saved token for http://127.0.0.1:9; run `cetra-cli login --url http://127.0.0.1:9`",
      code: Exit.SignIn,
    });
  });

  it("refuses bad arguments with Exit.Usage before any request", async () => {
    const { config, cwd } = await dirs();
    const env = { XDG_CONFIG_HOME: config, CETRA_TOKEN: PAT };
    const base = ["--url", "http://127.0.0.1:9", "--project", "acme/demo"];
    const usage = async (args: string[]) => {
      const error = await failureOf(query(args, fakeIo(env, cwd).io, deps));
      expect(error.code, args.join(" ")).toBe(Exit.Usage);
      return error.message;
    };

    expect(await usage(["SELECT 1", ...base, "--from", "2026-01-01T00:00:00Z"])).toBe(
      "--from and --to need each other",
    );
    expect(await usage(["SELECT 1", ...base, "--to", "2026-01-01T00:00:00Z"])).toBe(
      "--from and --to need each other",
    );
    expect(
      await usage([
        "SELECT 1",
        ...base,
        "--since",
        "1h",
        "--from",
        "2026-01-01T00:00:00Z",
        "--to",
        "2026-01-02T00:00:00Z",
      ]),
    ).toBe("--since cannot be combined with --from and --to");
    expect(await usage(["SELECT 1", ...base, "--limit", "0"])).toBe(
      "--limit expects a whole number from 1 to 10000",
    );
    expect(await usage(["SELECT 1", ...base, "--limit", "10001"])).toBe(
      "--limit expects a whole number from 1 to 10000",
    );
    expect(await usage(["SELECT 1", ...base, "--limit", "+5"])).toBe(
      "--limit expects a whole number from 1 to 10000",
    );
    expect(await usage(["SELECT 1", ...base, "--since", "0h"])).toBe(
      "--since expects a duration such as 30m, 24h or 7d",
    );
    expect(await usage([...base])).toBe("query takes one SQL statement in quotes");
    expect(await usage(["SELECT 1", "SELECT 2", ...base])).toBe(
      "query takes one SQL statement in quotes",
    );
    expect(await usage(["   ", ...base])).toBe("the query is empty");
  });

  it("prints its help on stderr and returns 0", async () => {
    const { config, cwd } = await dirs();
    const { io, stderr } = fakeIo({ XDG_CONFIG_HOME: config }, cwd);

    expect(await query(["--help"], io, deps)).toBe(Exit.Ok);
    expect(stderr.join("\n")).toContain("Usage: cetra-cli query [options]");
  });
});
