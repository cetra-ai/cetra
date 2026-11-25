import { execFileSync, spawn } from "node:child_process";
import { cpSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

/** Below the runner's own program timeout (PROGRAM_TIMEOUT, 120 s, in tools/capture/src/run.rs). */
const AGENT_TIMEOUT_MS = 90_000;

/** After SIGTERM to the agent's process group, the group gets SIGKILL this long later. */
const KILL_GRACE_MS = 5_000;

const projectRoot = fileURLToPath(new URL(".", import.meta.url));

export interface AgentRun {
  /** Executable under node_modules/.bin, e.g. "claude", or an absolute path to one. */
  bin: string;
  args: string[];
  /** The child's whole environment besides PATH, HOME, TERM, LANG and TZ. */
  env: Record<string, string>;
  /** Files to write before the run, relative to the isolated HOME. */
  homeFiles: Record<string, string>;
  /** Directories copied recursively into the isolated HOME before the run: the name relative to HOME, the value an absolute source path. */
  homeDirs?: Record<string, string>;
  /** Files written into the repo before the run, relative to the repo. */
  repoFiles?: Record<string, string>;
}

/** Paths of one run, for building config files before calling runAgent. */
export interface AgentPaths {
  home: string;
  repo: string;
}

function caseDir(): string {
  const scenarioCase = process.env.CETRA_CAPTURE_CASE;
  if (scenarioCase === undefined) {
    throw new Error("CETRA_CAPTURE_CASE is unset");
  }
  const repoRoot = path.resolve(projectRoot, "../../..");
  return path.join(repoRoot, "target", "capture-agents", scenarioCase.replaceAll("/", "__"));
}

export function agentPaths(): AgentPaths {
  const dir = caseDir();
  return { home: path.join(dir, "home"), repo: path.join(dir, "repo") };
}

function pathVariable(): string {
  const value = process.env.PATH;
  if (value === undefined) {
    throw new Error("PATH is unset");
  }
  return value;
}

export async function runAgent(run: AgentRun): Promise<void> {
  const { home, repo } = agentPaths();
  rmSync(caseDir(), { recursive: true, force: true });
  mkdirSync(home, { recursive: true });
  mkdirSync(repo, { recursive: true });

  for (const [name, source] of Object.entries(run.homeDirs ?? {})) {
    cpSync(source, path.join(home, name), { recursive: true });
  }

  for (const [name, content] of Object.entries(run.homeFiles)) {
    const file = path.join(home, name);
    mkdirSync(path.dirname(file), { recursive: true });
    writeFileSync(file, content, { mode: name.endsWith(".sh") ? 0o755 : 0o600 });
  }

  for (const [name, content] of Object.entries(run.repoFiles ?? {})) {
    const file = path.join(repo, name);
    mkdirSync(path.dirname(file), { recursive: true });
    writeFileSync(file, content);
  }

  const gitEnv = { PATH: pathVariable(), HOME: home, GIT_CONFIG_NOSYSTEM: "1" };
  const gitCommands: string[][] = [
    ["init", "-q", "-b", "feature/capture"],
    ["config", "user.email", "capture@example.com"],
    ["config", "user.name", "capture"],
    ["remote", "add", "origin", "https://github.com/acme/widgets.git"],
    ["commit", "-q", "--allow-empty", "-m", "init"],
  ];
  for (const args of gitCommands) {
    execFileSync("git", args, {
      cwd: repo,
      env: gitEnv,
      stdio: ["ignore", "ignore", "inherit"],
    });
  }

  // The agent's stdout goes to this process's stderr, so it never mixes with
  // anything the runner reads. The environment is explicit: the runner sets
  // OTEL_SERVICE_NAME, which would rename the agent if it were inherited.
  const bin = path.isAbsolute(run.bin)
    ? run.bin
    : path.join(projectRoot, "node_modules", ".bin", run.bin);
  const env = {
    PATH: pathVariable(),
    HOME: home,
    TERM: "dumb",
    LANG: "C.UTF-8",
    TZ: "UTC",
    ...run.env,
  };

  await new Promise<void>((resolve, reject) => {
    let timedOut = false;
    // Detached, the agent leads its own process group, so a group signal also
    // reaches the subprocesses it started.
    const child = spawn(bin, run.args, {
      cwd: repo,
      detached: true,
      stdio: ["ignore", 2, "inherit"],
      env,
    });
    const signalGroup = (signal: NodeJS.Signals): void => {
      if (child.pid === undefined) {
        return;
      }
      try {
        process.kill(-child.pid, signal);
      } catch {
        // The group is already gone.
      }
    };
    let killTimer: NodeJS.Timeout | undefined;
    const timer = setTimeout(() => {
      timedOut = true;
      signalGroup("SIGTERM");
      killTimer = setTimeout(() => signalGroup("SIGKILL"), KILL_GRACE_MS);
    }, AGENT_TIMEOUT_MS);
    const clearTimers = (): void => {
      clearTimeout(timer);
      clearTimeout(killTimer);
    };
    child.on("error", (error) => {
      clearTimers();
      const code = (error as NodeJS.ErrnoException).code ?? "unknown error";
      reject(new Error(`agent could not start (${code})`));
    });
    child.on("exit", (code, signal) => {
      clearTimers();
      if (timedOut) {
        reject(new Error("agent timed out"));
      } else if (code === 0) {
        resolve();
      } else {
        reject(new Error(`agent exited with ${code ?? signal}`));
      }
    });
  });
}
