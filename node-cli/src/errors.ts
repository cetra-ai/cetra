import type { execFile, spawn } from "node:child_process";

/** Exit codes (design § `@cetra-ai/cli`, D-AO-15). */
export const Exit = {
  Ok: 0,
  Error: 1,
  Usage: 2,
  Unreachable: 3,
  SignIn: 4,
  Write: 5,
  Query: 6,
} as const;
export type ExitCode = (typeof Exit)[keyof typeof Exit];

/** An error that ends the command with a message on stderr and an exit code. */
export class CliError extends Error {
  constructor(
    message: string,
    readonly code: ExitCode = Exit.Error,
  ) {
    super(message);
    this.name = "CliError";
  }
}

/** Output takes one line at a time, without its newline; realIo() adds it. */
export interface Io {
  stdout: (line: string) => void;
  stderr: (line: string) => void;
  env: NodeJS.ProcessEnv;
  cwd: string;
}

/** Test seams; production passes nothing. sleep and now: the device flow and the wait for a local
 *  server; skillSource: the skill to copy; spawn and execFile: the browser and the local server. */
export interface Deps {
  sleep?: (ms: number) => Promise<void>;
  now?: () => number; // milliseconds, like Date.now
  skillSource?: string;
  spawn?: typeof spawn;
  execFile?: typeof execFile;
}

/** The real Io: process.stdout / process.stderr with "\n" added, process.env, process.cwd(). A reader
 *  that stops early (`cetra-cli query ... | head`) ends the command with 0, as the Rust CLI does. */
export function realIo(): Io {
  process.stdout.once("error", (error: NodeJS.ErrnoException) => {
    if (error.code === "EPIPE") process.exit(Exit.Ok);
    throw error;
  });
  return {
    stdout: (line) => {
      process.stdout.write(`${line}\n`);
    },
    stderr: (line) => {
      process.stderr.write(`${line}\n`);
    },
    env: process.env,
    cwd: process.cwd(),
  };
}

/** Characters to strip or refuse: one place, so Biome's noControlCharactersInRegex is never hit. */
export const CONTROL = /\p{Cc}/gu;
