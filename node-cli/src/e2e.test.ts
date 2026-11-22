// End-to-end: `cetra-cli init` against a running `cetra serve` (`just e2e`).
// tools/e2e/run.sh starts the server, builds the package and sets CETRA_E2E_BASE_URL and the
// admin credentials; without CETRA_E2E_BASE_URL the suite is skipped.

import { execFile, spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { afterAll, describe, expect, it } from "vitest";

const BASE = process.env.CETRA_E2E_BASE_URL ?? "";
const CLI = fileURLToPath(new URL("../dist/cli.js", import.meta.url));
const CODE_LINE = /^Code: ([A-Z]{4}-[A-Z]{4})\n/m;
const SPAN_NAME = "cli-e2e";
const PROJECT = "acme/e2e";
const REQUEST_TIMEOUT_MS = 10_000;
const QUERY_LIMIT_MS = 30_000;

const execute = promisify(execFile);
const temporary: string[] = [];

afterAll(async () => {
  for (const dir of temporary.splice(0)) await rm(dir, { recursive: true, force: true });
});

interface Captured {
  code: number | null;
  stdout: string;
  stderr: string;
}

async function tempDir(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "cetra-cli-e2e-"));
  temporary.push(dir);
  return dir;
}

/** The environment of every child: a temporary config directory, and no variables that would point
 *  git at another repository or make the CLI use another server, project or token. */
function childEnv(config: string): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env, XDG_CONFIG_HOME: config };
  for (const name of ["GIT_DIR", "GIT_INDEX_FILE", "CETRA_URL", "CETRA_PROJECT", "CETRA_TOKEN"]) {
    delete env[name];
  }
  return env;
}

/** Runs the CLI to its end and collects its output. */
function runCli(args: string[], cwd: string, env: NodeJS.ProcessEnv): Promise<Captured> {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [CLI, ...args], {
      cwd,
      env,
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8").on("data", (chunk: string) => {
      stdout += chunk;
    });
    child.stderr.setEncoding("utf8").on("data", (chunk: string) => {
      stderr += chunk;
    });
    child.on("error", reject);
    child.on("close", (code) => resolve({ code, stdout, stderr }));
  });
}

function adminCredentials(): { email: string; password: string } {
  const email = process.env.CETRA_E2E_ADMIN_EMAIL;
  const password = process.env.CETRA_E2E_ADMIN_PASSWORD;
  if (!email || !password) {
    throw new Error("CETRA_E2E_ADMIN_EMAIL and CETRA_E2E_ADMIN_PASSWORD must be set");
  }
  return { email, password };
}

/** Signs in as the admin and returns the `cetra_session` cookie as a `Cookie` header value. */
async function sessionCookie(): Promise<string> {
  const { email, password } = adminCredentials();
  const answer = await fetch(`${BASE}/api/auth/login`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Origin: BASE },
    body: JSON.stringify({ email, password }),
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });
  if (!answer.ok) throw new Error(`the admin login was refused: HTTP ${answer.status}`);
  const cookie = answer.headers.getSetCookie().find((value) => value.startsWith("cetra_session="));
  if (cookie === undefined) throw new Error("the admin login set no cetra_session cookie");
  return cookie.split(";")[0] ?? "";
}

/** The id of the project `acme/e2e`, from the org's project list. */
async function projectId(cookie: string): Promise<string> {
  const answer = await fetch(`${BASE}/api/v1/orgs/acme/projects`, {
    headers: { Cookie: cookie },
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });
  if (!answer.ok) throw new Error(`the project list was refused: HTTP ${answer.status}`);
  const page = (await answer.json()) as { data: { id: string; slug: string }[] };
  const id = page.data.find((item) => item.slug === "e2e")?.id;
  if (id === undefined) throw new Error(`${PROJECT} is not in the project list`);
  return id;
}

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

describe.skipIf(BASE === "")("cetra-cli init end to end", () => {
  it("signs in with the device flow, sends a span with the written key and reads it back with query", async () => {
    const dir = await tempDir();
    const config = await tempDir();
    await execute("git", ["init", "-q"], { cwd: dir, env: childEnv(config) });
    const env = childEnv(config);

    // 1. Start init and wait for its sign-in code on stderr.
    const child = spawn(
      process.execPath,
      [
        CLI,
        "init",
        "--url",
        BASE,
        "--dir",
        dir,
        "--yes",
        "--no-browser",
        "--json",
        "--project",
        PROJECT,
      ],
      { cwd: dir, env, stdio: ["ignore", "pipe", "pipe"] },
    );
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8").on("data", (chunk: string) => {
      stdout += chunk;
    });
    const exited = new Promise<number | null>((resolve, reject) => {
      child.on("error", reject);
      child.on("close", resolve);
    });
    const userCode = await new Promise<string>((resolve, reject) => {
      child.stderr.setEncoding("utf8").on("data", (chunk: string) => {
        stderr += chunk;
        const match = CODE_LINE.exec(stderr);
        if (match?.[1] !== undefined) resolve(match[1]);
      });
      exited.then(
        () => reject(new Error(`init exited before its sign-in code: ${stderr}`)),
        reject,
      );
    });

    // 2. Approve the code as the admin, for the project acme/e2e.
    const cookie = await sessionCookie();
    const approved = await fetch(`${BASE}/api/auth/device/${userCode}/approve`, {
      method: "POST",
      headers: { "Content-Type": "application/json", Origin: BASE, Cookie: cookie },
      body: JSON.stringify({ project_id: await projectId(cookie) }),
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    });
    expect(approved.status).toBe(204);

    // 3. init finishes with code 0 and reports the project.
    expect(await exited).toBe(0);
    const summary = JSON.parse(stdout) as { project: string };
    expect(summary.project).toBe(PROJECT);

    // 4. Send one span with the key init wrote.
    const envText = await readFile(join(dir, ".env"), "utf8");
    const key = /^CETRA_API_KEY=(.+)$/m.exec(envText)?.[1]?.trim() ?? "";
    expect(key).toMatch(/^sk-cetra-/);
    const traceId = randomBytes(16).toString("hex");
    const spanId = randomBytes(8).toString("hex");
    const startNs = BigInt(Date.now()) * 1_000_000n;
    const sent = await fetch(`${BASE}/v1/traces`, {
      method: "POST",
      headers: { "Content-Type": "application/json", Authorization: `Bearer ${key}` },
      body: JSON.stringify({
        resourceSpans: [
          {
            scopeSpans: [
              {
                spans: [
                  {
                    traceId,
                    spanId,
                    name: SPAN_NAME,
                    startTimeUnixNano: startNs.toString(),
                    endTimeUnixNano: (startNs + 1_000_000n).toString(),
                  },
                ],
              },
            ],
          },
        ],
      }),
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    });
    expect(sent.ok).toBe(true);

    // 5. query reads the span back; the server commits spans about every 5 s.
    const sql = `SELECT trace_id FROM traces WHERE name = '${SPAN_NAME}'`;
    const deadline = Date.now() + QUERY_LIMIT_MS;
    let found = false;
    while (!found) {
      const answer = await runCli(["query", sql, "--since", "1h", "--json"], dir, env);
      if (answer.code !== 0) {
        throw new Error(`query exited ${answer.code}: ${answer.stderr}`);
      }
      const result = JSON.parse(answer.stdout) as { rows: { trace_id?: unknown }[] };
      found = result.rows.some((row) => row.trace_id === traceId);
      if (!found) {
        if (Date.now() >= deadline) {
          throw new Error(`span ${traceId} was not queryable within 30 s`);
        }
        await sleep(1000);
      }
    }

    // 6. The skill is installed in both agent folders of the project directory.
    await expect(stat(join(dir, ".claude/skills/cetra/SKILL.md"))).resolves.toBeDefined();
    await expect(stat(join(dir, ".agents/skills/cetra/SKILL.md"))).resolves.toBeDefined();
  }, 90_000);
});
