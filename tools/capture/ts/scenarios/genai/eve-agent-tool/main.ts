import { spawn } from "node:child_process";
import { mkdirSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { requiredEnv, writeMeta } from "../../../harness.ts";

// The pinned eve install IN9.0 staged as a pnpm tool project (eve needs AI SDK 7,
// while the capture project has 6). Its node_modules is linked into the scenario
// folder for the run, so the agent files eve bundles resolve the same packages.
const EVE_TOOLS = "eve/1.0.1/node_modules";

const POLL_TIMEOUT_MS = 30_000;
const STREAM_TIMEOUT_MS = 60_000;
// eve exports its last OTLP batch about 5 s after the last model call, so the
// turn is given 7 s to settle before the dev server is stopped.
const SETTLE_MS = 7_000;
const EXIT_TIMEOUT_MS = 4_000;

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

async function waitForServer(url: string): Promise<void> {
  const deadline = Date.now() + POLL_TIMEOUT_MS;
  for (;;) {
    if (Date.now() > deadline) {
      throw new Error(`eve did not answer 200 on ${url} within ${POLL_TIMEOUT_MS} ms`);
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

/** Reads the session stream until the turn's `turn.settled` line. */
async function waitTurnSettled(url: string): Promise<void> {
  const controller = new AbortController();
  const timer = setTimeout(() => {
    controller.abort();
  }, STREAM_TIMEOUT_MS);
  try {
    const response = await fetch(url, { signal: controller.signal });
    const reader = response.body?.getReader();
    if (reader === undefined) {
      throw new Error("the session stream has no body");
    }
    const decoder = new TextDecoder();
    let buffer = "";
    for (;;) {
      const { done, value } = await reader.read();
      if (done) {
        throw new Error("the session stream ended before the turn settled");
      }
      buffer += decoder.decode(value, { stream: true });
      let newline = buffer.indexOf("\n");
      while (newline >= 0) {
        const line = buffer.slice(0, newline);
        buffer = buffer.slice(newline + 1);
        if (line.includes('"type":"turn.settled"')) {
          await reader.cancel();
          return;
        }
        newline = buffer.indexOf("\n");
      }
    }
  } finally {
    clearTimeout(timer);
  }
}

async function main(): Promise<void> {
  const tools = requiredEnv("CETRA_CAPTURE_TOOLS");
  const eveBin = path.join(tools, EVE_TOOLS, "eve/bin/eve.js");
  // The work directory holds hello.txt beside the config and temp directories
  // eve is pointed at, so a run writes nothing outside the scenario folder.
  const work = path.join(scenarioDir, "work");
  rmSync(work, { recursive: true, force: true });
  mkdirSync(path.join(work, "xdg"), { recursive: true });
  mkdirSync(path.join(work, "tmp"), { recursive: true });
  writeFileSync(path.join(work, "hello.txt"), "hello from cetra\n");
  const port = await freePort();
  symlinkSync(path.join(tools, EVE_TOOLS), path.join(scenarioDir, "node_modules"), "dir");
  // An explicit environment: no OTEL_* variable reaches eve, so the service name
  // stays capture-eve (the package.json name) and the exporter URL is the one
  // the instrumentation file reads.
  const child = spawn(process.execPath, [eveBin, "dev", "--no-ui", "--port", String(port)], {
    cwd: scenarioDir,
    stdio: ["ignore", 2, "inherit"],
    env: {
      PATH: requiredEnv("PATH"),
      HOME: requiredEnv("HOME"),
      TZ: requiredEnv("TZ"),
      LANG: requiredEnv("LANG"),
      CETRA_CAPTURE_MOCK_URL: requiredEnv("CETRA_CAPTURE_MOCK_URL"),
      CETRA_CAPTURE_OTLP_ENDPOINT: requiredEnv("CETRA_CAPTURE_OTLP_ENDPOINT"),
      OPENAI_API_KEY: requiredEnv("OPENAI_API_KEY"),
      CAPTURE_EVE_WORK: work,
      XDG_CONFIG_HOME: path.join(work, "xdg"),
      TMPDIR: path.join(work, "tmp"),
    },
  });
  let startError: Error | undefined;
  const exited = new Promise<void>((resolve) => {
    child.once("exit", () => {
      resolve();
    });
    child.once("error", (error: NodeJS.ErrnoException) => {
      startError = new Error(`eve could not start (${error.code ?? "unknown error"})`);
      resolve();
    });
  });
  try {
    await waitForServer(`http://127.0.0.1:${port}/`);
    const response = await fetch(`http://127.0.0.1:${port}/eve/v1/session`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        message: "Read hello.txt with the read_file tool and say what it says.",
      }),
    });
    if (response.status !== 202) {
      throw new Error(`the session POST answered ${response.status}`);
    }
    const body = (await response.json()) as { ok: boolean; sessionId: string };
    await waitTurnSettled(`http://127.0.0.1:${port}/eve/v1/session/${body.sessionId}/stream`);
    await sleep(SETTLE_MS);
    child.kill("SIGINT");
    await Promise.race([exited, sleep(EXIT_TIMEOUT_MS)]);
    child.kill("SIGKILL");
    await exited;
    if (startError !== undefined) {
      throw startError;
    }
    rmSync(work, { recursive: true, force: true });
    writeMeta({
      eve: "../../../../target/capture-tools/eve/1.0.1/node_modules/eve",
      "@vercel/otel": "../../../../target/capture-tools/eve/1.0.1/node_modules/@vercel/otel",
    });
  } finally {
    if (child.exitCode === null && child.signalCode === null) {
      child.kill("SIGKILL");
    }
    rmSync(path.join(scenarioDir, "node_modules"), { force: true });
  }
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : "Error";
  process.stderr.write(`capture scenario failed: ${message}\n`);
  process.exitCode = 1;
});
