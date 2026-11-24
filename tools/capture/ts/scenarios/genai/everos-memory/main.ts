import { spawn } from "node:child_process";
import { mkdirSync, rmSync } from "node:fs";
import { createServer } from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { requiredEnv, writeMeta } from "../../../harness.ts";

// The pinned EverOS environment fetch-tools.sh installs as a uv tool project
// (IN9.6). Its console script runs under the venv's own python.
const EVEROS_BIN = "everos/1.7.0/bin/everos";

const HEALTH_TIMEOUT_MS = 60_000;
const EXIT_TIMEOUT_MS = 4_000;
// The flush's extraction attempts finish in a background job right after its
// HTTP answer; the search's embedding request must not overtake them, or it
// would take a chat turn's place at the mock.
const EXTRACTION_DRAIN_MS = 2_000;
// The OTLP batch is flushed when the server shuts down; a short settle keeps
// the SIGINT away from an export that is still being written.
const SETTLE_MS = 3_000;

const scenarioDir = fileURLToPath(new URL(".", import.meta.url));

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}

function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.unref();
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (address === null || typeof address === "string") {
        server.close();
        reject(new Error("the free port could not be read"));
        return;
      }
      const { port } = address;
      server.close(() => {
        resolve(port);
      });
    });
  });
}

async function waitForHealth(url: string): Promise<void> {
  const deadline = Date.now() + HEALTH_TIMEOUT_MS;
  for (;;) {
    if (Date.now() > deadline) {
      throw new Error(`everos did not answer 200 on ${url} within ${HEALTH_TIMEOUT_MS} ms`);
    }
    try {
      const response = await fetch(url);
      await response.body?.cancel();
      if (response.status === 200) {
        return;
      }
    } catch {
      // Not listening yet.
    }
    await sleep(200);
  }
}

async function main(): Promise<void> {
  const tools = requiredEnv("CETRA_CAPTURE_TOOLS");
  const everosBin = path.join(tools, EVEROS_BIN);
  const apiKey = requiredEnv("OPENAI_API_KEY");
  const mockUrl = requiredEnv("CETRA_CAPTURE_MOCK_URL");
  const endpoint = requiredEnv("CETRA_CAPTURE_OTLP_ENDPOINT");
  // The work directory holds EverOS's root (EVEROS_ROOT, where `everos init`
  // writes its config) beside the temp directories the server is pointed at,
  // so a run writes nothing outside the scenario folder.
  const work = path.join(scenarioDir, "work");
  const root = path.join(work, "root");
  const xdgHome = path.join(work, "xdg");
  const tmp = path.join(work, "tmp");
  const env: NodeJS.ProcessEnv = {
    PATH: requiredEnv("PATH"),
    HOME: requiredEnv("HOME"),
    TZ: requiredEnv("TZ"),
    LANG: requiredEnv("LANG"),
    EVEROS_ROOT: root,
    EVEROS_LLM__MODEL: "openai/gpt-4o-mini",
    EVEROS_LLM__API_KEY: apiKey,
    EVEROS_LLM__BASE_URL: mockUrl + "/v1",
    EVEROS_EMBEDDING__MODEL: "text-embedding-3-small",
    EVEROS_EMBEDDING__API_KEY: apiKey,
    EVEROS_EMBEDDING__BASE_URL: mockUrl + "/v1",
    EVEROS_EMBEDDING__DIM: "1024",
    EVEROS_OBSERVABILITY__ENABLED: "true",
    EVEROS_OBSERVABILITY__ENDPOINT: endpoint,
    EVEROS_OBSERVABILITY__CAPTURE_CONTENT: "true",
    TIKTOKEN_CACHE_DIR: path.join(tools, "tiktoken"),
    XDG_CONFIG_HOME: xdgHome,
    TMPDIR: tmp,
  };
  rmSync(work, { recursive: true, force: true });
  mkdirSync(xdgHome, { recursive: true });
  mkdirSync(tmp, { recursive: true });
  await new Promise<void>((resolve, reject) => {
    const init = spawn(everosBin, ["init"], { cwd: work, stdio: ["ignore", 2, 2], env });
    init.once("error", (error: NodeJS.ErrnoException) => {
      reject(new Error(`everos init could not start (${error.code ?? "unknown error"})`));
    });
    init.once("exit", (code) => {
      if (code === 0) {
        resolve();
        return;
      }
      reject(new Error(`everos init exited with ${code ?? "no status"}`));
    });
  });
  const port = await freePort();
  const server = spawn(everosBin, ["server", "start"], {
    cwd: work,
    stdio: ["ignore", 2, "inherit"],
    env: { ...env, EVEROS_API__PORT: String(port) },
  });
  let startError: Error | undefined;
  const exited = new Promise<void>((resolve) => {
    server.once("exit", () => {
      resolve();
    });
    server.once("error", (error: NodeJS.ErrnoException) => {
      startError = new Error(`everos could not start (${error.code ?? "unknown error"})`);
      resolve();
    });
  });
  try {
    await waitForHealth(`http://127.0.0.1:${port}/health`);
    const add = await fetch(`http://127.0.0.1:${port}/api/v2/memory/add`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        session_id: "capture-s1",
        messages: [
          {
            sender_id: "capture-user",
            role: "user",
            timestamp: 1732358400000,
            content: "hello from cetra",
          },
        ],
      }),
    });
    if (add.status !== 200) {
      throw new Error(`the add POST answered ${add.status}`);
    }
    await add.body?.cancel();
    // The flush's extraction spans run with the mock's one fixed answer, which
    // only the boundary call's Reply model accepts, so its HTTP envelope is an
    // error by design; the call is made for the spans it produces.
    const flush = await fetch(`http://127.0.0.1:${port}/api/v2/memory/flush`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ session_id: "capture-s1" }),
    });
    await flush.body?.cancel();
    await sleep(EXTRACTION_DRAIN_MS);
    const search = await fetch(`http://127.0.0.1:${port}/api/v2/memory/search`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ query: "what did the user say?", user_id: "capture-user" }),
    });
    if (search.status !== 200) {
      throw new Error(`the search POST answered ${search.status}`);
    }
    await search.body?.cancel();
    await sleep(SETTLE_MS);
    server.kill("SIGINT");
    await Promise.race([exited, sleep(EXIT_TIMEOUT_MS)]);
    server.kill("SIGKILL");
    await exited;
    if (startError !== undefined) {
      throw startError;
    }
    writeMeta({}, { everos: "1.7.0" });
  } finally {
    if (server.exitCode === null && server.signalCode === null) {
      server.kill("SIGKILL");
    }
    rmSync(work, { recursive: true, force: true });
  }
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : "Error";
  process.stderr.write(`capture scenario failed: ${message}\n`);
  process.exitCode = 1;
});
