#!/usr/bin/env node
import { realpathSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { CliError, Exit, type ExitCode, type Io, realIo } from "./errors.js";
import { VERSION } from "./version.js";

/** Commands register here: C3 (login), C4 (query) and C5c (init). C1 registers none. */
export const COMMANDS: Record<string, (args: string[], io: Io) => Promise<ExitCode>> = {};

const USAGE = `Usage: cetra-cli <command> [options]

Commands:
  init     Sign in, pick a project, write .env and install the cetra skill
  query    Run a SQL query on the linked project
  login    Sign in again and save a new token

Run \`cetra-cli <command> --help\` for a command's options.`;

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export async function main(
  argv: string[],
  io: Io,
  commands: typeof COMMANDS = COMMANDS,
): Promise<ExitCode> {
  const [first, ...rest] = argv;
  if (first === undefined) {
    io.stderr(USAGE);
    return Exit.Usage;
  }
  if (first === "--version") {
    io.stdout(VERSION);
    return Exit.Ok;
  }
  if (first === "--help" || first === "-h") {
    io.stderr(USAGE);
    return Exit.Ok;
  }
  if (first.startsWith("-")) {
    io.stderr(`cetra-cli: unknown option ${first}`);
    io.stderr(USAGE);
    return Exit.Usage;
  }
  const command = Object.hasOwn(commands, first) ? commands[first] : undefined;
  if (command === undefined) {
    io.stderr(`cetra-cli: unknown command ${first}`);
    io.stderr(USAGE);
    return Exit.Usage;
  }
  try {
    return await command(rest, io);
  } catch (error) {
    io.stderr(`cetra-cli: ${message(error)}`);
    return error instanceof CliError ? error.code : Exit.Error;
  }
}

/** True when this module is the file node was started with (through the npm bin symlink too). */
function isEntryPoint(): boolean {
  const entry = process.argv[1];
  if (entry === undefined) return false;
  try {
    return import.meta.url === pathToFileURL(realpathSync(entry)).href;
  } catch {
    return false;
  }
}

if (isEntryPoint()) {
  main(process.argv.slice(2), realIo()).then((code) => {
    process.exitCode = code;
  });
}
