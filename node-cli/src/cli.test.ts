import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { optionsHelp, parseOptions } from "./args.js";
import { main } from "./cli.js";
import { CliError, Exit, type ExitCode, type Io } from "./errors.js";
import { VERSION } from "./version.js";

function fakeIo(): { io: Io; stdout: string[]; stderr: string[] } {
  const stdout: string[] = [];
  const stderr: string[] = [];
  const io: Io = {
    stdout: (line) => stdout.push(line),
    stderr: (line) => stderr.push(line),
    env: {},
    cwd: "/",
  };
  return { io, stdout, stderr };
}

async function run(argv: string[], commands?: Parameters<typeof main>[2]) {
  const captured = fakeIo();
  const code: ExitCode = await main(argv, captured.io, commands);
  return { code, stdout: captured.stdout, stderr: captured.stderr };
}

describe("cetra-cli", () => {
  it("--version prints the version on stdout and returns 0", async () => {
    const { code, stdout, stderr } = await run(["--version"]);
    expect(code).toBe(Exit.Ok);
    expect(stdout).toEqual([VERSION]);
    expect(stderr).toEqual([]);
  });

  it("VERSION equals the package.json version", () => {
    const pkg = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8")) as {
      version: string;
    };
    expect(VERSION).toBe(pkg.version);
  });

  it("no argument prints the usage on stderr and returns 2", async () => {
    const { code, stdout, stderr } = await run([]);
    expect(code).toBe(Exit.Usage);
    expect(stdout).toEqual([]);
    expect(stderr.join("\n")).toContain("Usage: cetra-cli <command> [options]");
  });

  it("--help prints the usage on stderr and returns 0", async () => {
    for (const flag of ["--help", "-h"]) {
      const { code, stderr } = await run([flag]);
      expect(code).toBe(Exit.Ok);
      expect(stderr.join("\n")).toContain("Commands:");
    }
  });

  it("an unknown command prints its name and the usage and returns 2", async () => {
    const { code, stderr } = await run(["frob"]);
    expect(code).toBe(Exit.Usage);
    expect(stderr[0]).toBe("cetra-cli: unknown command frob");
    expect(stderr.join("\n")).toContain("Usage: cetra-cli");
  });

  it("an unknown option before the command returns 2", async () => {
    const { code, stderr } = await run(["--frob"]);
    expect(code).toBe(Exit.Usage);
    expect(stderr[0]).toBe("cetra-cli: unknown option --frob");
  });

  it("a CliError from a command prints its message and returns its code", async () => {
    const { code, stderr } = await run(["fake"], {
      fake: async () => {
        throw new CliError("x", Exit.Write);
      },
    });
    expect(code).toBe(Exit.Write);
    expect(stderr).toEqual(["cetra-cli: x"]);
  });

  it("a plain Error from a command prints its message and returns 1 with no stack", async () => {
    const { code, stderr } = await run(["fake"], {
      fake: async () => {
        throw new Error("y");
      },
    });
    expect(code).toBe(Exit.Error);
    expect(stderr).toEqual(["cetra-cli: y"]);
  });

  it("passes the arguments after the command name to the command", async () => {
    let seen: string[] | undefined;
    const { code } = await run(["fake", "--json", "x"], {
      fake: async (args) => {
        seen = args;
        return Exit.Ok;
      },
    });
    expect(code).toBe(Exit.Ok);
    expect(seen).toEqual(["--json", "x"]);
  });
});

describe("parseOptions", () => {
  it("turns an unknown option into Exit.Usage", () => {
    try {
      parseOptions(["--nope"], { json: { type: "boolean" } });
      expect.unreachable("parseOptions should throw");
    } catch (error) {
      expect(error).toBeInstanceOf(CliError);
      expect((error as CliError).code).toBe(Exit.Usage);
    }
  });

  it("parses known options and keeps positionals", () => {
    const { values, positionals } = parseOptions(["select 1", "--json"], {
      json: { type: "boolean" },
    });
    expect(values.json).toBe(true);
    expect(positionals).toEqual(["select 1"]);
  });
});

describe("optionsHelp", () => {
  it("lists each option on its own line", () => {
    const text = optionsHelp("query", "Run a SQL query.", [
      ["--json", "Print JSON on stdout"],
      ["-h, --help", "Show this help"],
    ]);
    expect(text).toContain("Usage: cetra-cli query [options]");
    expect(text).toContain("  --json      Print JSON on stdout");
    expect(text).toContain("  -h, --help  Show this help");
  });
});
