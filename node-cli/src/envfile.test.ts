import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { ignoreInGit, readVariable, setVariables, writeEnvFile } from "./envfile.js";
import { CliError, Exit } from "./errors.js";

const temporary: string[] = [];

afterEach(async () => {
  for (const dir of temporary.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

async function tempDir(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "cetra-env-"));
  temporary.push(dir);
  return dir;
}

/** A git environment with GIT_DIR and GIT_INDEX_FILE removed, as the CLI runs git. */
function gitEnv(): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env };
  delete env.GIT_DIR;
  delete env.GIT_INDEX_FILE;
  return env;
}

describe("setVariables", () => {
  it("replaces assignments in place and keeps comments, order and export", () => {
    const text = "# keep\nFOO=1\nexport CETRA_API_KEY=old\nBAR=2\n";
    expect(
      setVariables(text, {
        CETRA_API_KEY: "sk-cetra-new",
        CETRA_BASE_URL: "http://localhost:8080",
      }),
    ).toBe(
      "# keep\nFOO=1\nexport CETRA_API_KEY=sk-cetra-new\nBAR=2\nCETRA_BASE_URL=http://localhost:8080\n",
    );
  });

  it("keeps CRLF line endings", () => {
    expect(setVariables("A=1\r\nCETRA_API_KEY=old\r\n", { CETRA_API_KEY: "sk-x", B: "y" })).toBe(
      "A=1\r\nCETRA_API_KEY=sk-x\r\nB=y\r\n",
    );
  });

  it("keeps a leading byte order mark", () => {
    expect(setVariables("﻿A=1\n", { B: "2" })).toBe("﻿A=1\nB=2\n");
  });

  it("adds a header to a new file", () => {
    expect(
      setVariables(undefined, {
        CETRA_API_KEY: "sk-cetra-x",
        CETRA_BASE_URL: "http://localhost:8080",
      }),
    ).toBe(
      "# cetra (npx @cetra-ai/cli init)\nCETRA_API_KEY=sk-cetra-x\nCETRA_BASE_URL=http://localhost:8080\n",
    );
  });

  it("ends a file that lacked a final newline with one newline", () => {
    expect(setVariables("A=1", { B: "2" })).toBe("A=1\nB=2\n");
  });

  it("accepts an IPv6 URL", () => {
    expect(setVariables(undefined, { CETRA_BASE_URL: "http://[::1]:8080" })).toContain(
      "CETRA_BASE_URL=http://[::1]:8080\n",
    );
  });

  it("refuses a value that is not a plain word, URL or key", () => {
    for (const value of ["a b", 'x"y', "a\nb", "a#b", "$HOME"]) {
      const error = (() => {
        try {
          setVariables(undefined, { CETRA_BASE_URL: value });
          return undefined;
        } catch (caught) {
          return caught;
        }
      })();
      expect(error).toBeInstanceOf(CliError);
      expect((error as CliError).message).toBe("invalid value for CETRA_BASE_URL");
      expect((error as CliError).code).toBe(Exit.Write);
    }
  });
});

describe("readVariable", () => {
  it("reads the value without quotes, export or a CR", () => {
    expect(readVariable('CETRA_API_KEY="sk-x"\n', "CETRA_API_KEY")).toBe("sk-x");
    expect(readVariable("CETRA_API_KEY='sk-y'\n", "CETRA_API_KEY")).toBe("sk-y");
    expect(readVariable("export CETRA_API_KEY=sk-z\n", "CETRA_API_KEY")).toBe("sk-z");
    expect(readVariable("CETRA_API_KEY=sk-z\r\nB=1\r\n", "CETRA_API_KEY")).toBe("sk-z");
  });

  it("reads the first byte-order-marked line and the last of duplicates", () => {
    expect(readVariable("﻿CETRA_BASE_URL=http://a\n", "CETRA_BASE_URL")).toBe("http://a");
    expect(readVariable("X=1\nX=2\n", "X")).toBe("2");
  });

  it("returns undefined when the name is not assigned", () => {
    expect(readVariable("# CETRA_API_KEY=commented\n", "CETRA_API_KEY")).toBeUndefined();
  });
});

describe("writeEnvFile", () => {
  it.skipIf(process.platform === "win32")(
    "writes the file 0600 and leaves no temporary file",
    async () => {
      const dir = await tempDir();
      const path = join(dir, "nested", ".env");
      await writeEnvFile(path, "CETRA_API_KEY=sk-x\n");
      expect(await readFile(path, "utf8")).toBe("CETRA_API_KEY=sk-x\n");
      expect((await stat(path)).mode & 0o777).toBe(0o600);
      await expect(stat(`${path}.tmp`)).rejects.toThrow();
    },
  );

  it("replaces an existing file", async () => {
    const dir = await tempDir();
    const path = join(dir, ".env");
    await writeFile(path, "OLD=1\n");
    await writeEnvFile(path, "NEW=2\n");
    expect(await readFile(path, "utf8")).toBe("NEW=2\n");
  });

  it("refuses when the directory cannot be created, naming only the file", async () => {
    const dir = await tempDir();
    const blocker = join(dir, "blocker");
    await writeFile(blocker, "not a directory");
    const error = await writeEnvFile(join(blocker, ".env"), "x=1\n").catch(
      (caught: unknown) => caught,
    );
    expect(error).toBeInstanceOf(CliError);
    expect((error as CliError).message).toBe("could not write .env");
    expect((error as CliError).code).toBe(Exit.Write);
    expect((error as CliError).message).not.toContain(dir);
  });
});

describe("ignoreInGit", () => {
  it("adds the file to .gitignore once in a work tree", async () => {
    const dir = await tempDir();
    execFileSync("git", ["init", "-q"], { cwd: dir, env: gitEnv() });
    expect(await ignoreInGit(dir, ".env")).toBe(true);
    expect(await readFile(join(dir, ".gitignore"), "utf8")).toBe(".env\n");
    expect(await ignoreInGit(dir, ".env")).toBe(false);
    expect(await readFile(join(dir, ".gitignore"), "utf8")).toBe(".env\n");
  });

  it("keeps existing lines and adds a newline before the entry when one is missing", async () => {
    const dir = await tempDir();
    execFileSync("git", ["init", "-q"], { cwd: dir, env: gitEnv() });
    await writeFile(join(dir, ".gitignore"), "node_modules");
    expect(await ignoreInGit(dir, join(dir, ".env"))).toBe(true);
    expect(await readFile(join(dir, ".gitignore"), "utf8")).toBe("node_modules\n.env\n");
  });

  it("does nothing outside a work tree", async () => {
    const dir = await tempDir();
    expect(await ignoreInGit(dir, ".env")).toBe(false);
    await expect(stat(join(dir, ".gitignore"))).rejects.toThrow();
  });

  it("does nothing for an env file outside the directory", async () => {
    const dir = await tempDir();
    const other = await tempDir();
    await mkdir(join(other, "sub"), { recursive: true });
    execFileSync("git", ["init", "-q"], { cwd: dir, env: gitEnv() });
    expect(await ignoreInGit(dir, join(other, "sub", ".env"))).toBe(false);
  });
});
