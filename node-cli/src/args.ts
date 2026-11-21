import { type ParseArgsOptionsConfig, parseArgs } from "node:util";
import { CliError, Exit } from "./errors.js";

type Parsed<T extends ParseArgsOptionsConfig> = ReturnType<
  typeof parseArgs<{ args: string[]; options: T; strict: true; allowPositionals: true }>
>;

/** parseArgs with `strict: true` and positionals allowed; any parse error becomes
 *  CliError(<its message>, Exit.Usage). A command that takes no positional checks
 *  `positionals.length === 0` itself. */
export function parseOptions<const T extends ParseArgsOptionsConfig>(
  args: string[],
  options: T,
): Parsed<T> {
  try {
    return parseArgs({ args, options, strict: true, allowPositionals: true });
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new CliError(message, Exit.Usage);
  }
}

/** The options list of a command, one line per option, for `--help`. */
export function optionsHelp(command: string, summary: string, lines: [string, string][]): string {
  const width = Math.max(...lines.map(([flag]) => flag.length));
  const rows = lines.map(([flag, text]) => `  ${flag.padEnd(width)}  ${text}`);
  return [`Usage: cetra-cli ${command} [options]`, "", summary, "", "Options:", ...rows].join("\n");
}
