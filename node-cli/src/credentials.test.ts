import { mkdir, mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import {
  checkServerUrl,
  chosenServer,
  credentialsPath,
  loadCredentials,
  normalizeServerUrl,
  saveToken,
  serverUrl,
} from "./credentials.js";
import { CliError, Exit } from "./errors.js";

const BAD_URL = "--url must start with http:// or https://";
const CREDENTIALS = "the server URL must not contain credentials";
const temporary: string[] = [];

async function tempDir(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "cetra-cred-"));
  temporary.push(dir);
  return dir;
}

afterEach(async () => {
  for (const dir of temporary.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

function thrown(action: () => unknown): CliError {
  try {
    action();
  } catch (error) {
    if (error instanceof CliError) return error;
    throw error;
  }
  throw new Error("expected a CliError");
}

describe("credentialsPath", () => {
  it("uses the XDG config directory on Linux when it is absolute", () => {
    expect(credentialsPath({ XDG_CONFIG_HOME: "/x/cfg" }, "linux", "/home/u")).toBe(
      "/x/cfg/cetra/credentials.toml",
    );
  });

  it("falls back to ~/.config on Linux when XDG_CONFIG_HOME is unset, empty or relative", () => {
    for (const env of [{}, { XDG_CONFIG_HOME: "" }, { XDG_CONFIG_HOME: "rel/cfg" }]) {
      expect(credentialsPath(env, "linux", "/home/u")).toBe(
        "/home/u/.config/cetra/credentials.toml",
      );
    }
  });

  it("uses Application Support on macOS", () => {
    expect(credentialsPath({}, "darwin", "/Users/u")).toBe(
      "/Users/u/Library/Application Support/cetra/credentials.toml",
    );
  });

  it("uses APPDATA on Windows and refuses when it is missing", () => {
    expect(credentialsPath({ APPDATA: "C:\\Users\\u\\AppData\\Roaming" }, "win32", "C:\\h")).toBe(
      "C:\\Users\\u\\AppData\\Roaming\\cetra\\credentials.toml",
    );
    const error = thrown(() => credentialsPath({}, "win32", "C:\\h"));
    expect(error.message).toBe("could not find the config directory");
  });
});

describe("server URLs", () => {
  it("drops trailing slashes and surrounding space", () => {
    expect(normalizeServerUrl(" http://127.0.0.1:8080/// ")).toBe("http://127.0.0.1:8080");
  });

  it("accepts the URLs the Rust CLI accepts", () => {
    for (const url of ["http://[::1]:8080", "http://h?x=a@b", "http://h#@", "http://h/@path"]) {
      expect(() => checkServerUrl(url), url).not.toThrow();
    }
  });

  it("refuses control characters, bad schemes and credentials, without the URL in the message", () => {
    const cases: [string, string][] = [
      ["HTTPS://u@h", CREDENTIALS],
      ["http://u:p@ss@h", CREDENTIALS],
      ["\x01http://h", BAD_URL],
      ["http://h\x0f", BAD_URL],
      ["http://h\n", BAD_URL],
      ["http:/\t/user:pw@h", BAD_URL],
      ["ftp://h", BAD_URL],
      ["user:pw@h", BAD_URL],
      ["127.0.0.1:8080", BAD_URL],
    ];
    for (const [url, message] of cases) {
      const error = thrown(() => checkServerUrl(url));
      expect(error.message, url).toBe(message);
      expect(error.code, url).toBe(Exit.Usage);
      expect(error.message, url).not.toMatch(/user|pw/);
    }
  });

  it("trims, checks, then normalizes in serverUrl", () => {
    expect(serverUrl("  http://h:1/  ")).toBe("http://h:1");
    expect(serverUrl("\thttp://h")).toBe("http://h");
    expect(serverUrl("http://h\n")).toBe("http://h");
    expect(serverUrl("HTTPS://h")).toBe("HTTPS://h");
    expect(thrown(() => serverUrl("\x01http://h")).message).toBe(BAD_URL);
  });
});

describe("chosenServer", () => {
  it("uses the only saved server", () => {
    expect(chosenServer(new Map([["http://h:1", "pat-a"]]))).toBe("http://h:1");
  });

  it("refuses when nothing is saved, or several servers are", () => {
    expect(thrown(() => chosenServer(new Map())).message).toBe(
      "no saved server; run `cetra-cli login --url <URL>`",
    );
    const two = new Map([
      ["http://a:1", "pat-a"],
      ["http://b:1", "pat-b"],
    ]);
    expect(thrown(() => chosenServer(two)).message).toBe("more than one saved server; pass --url");
  });

  it("refuses a saved server URL that holds credentials", () => {
    const saved = new Map([["http://user:pw@h:1", "pat-a"]]);
    expect(thrown(() => chosenServer(saved)).message).toBe(CREDENTIALS);
  });
});

describe("credentials file", () => {
  it("reads a missing file as empty", async () => {
    const dir = await tempDir();
    expect((await loadCredentials(join(dir, "none", "credentials.toml"))).size).toBe(0);
  });

  it("round-trips one token and writes the Rust format", async () => {
    const dir = await tempDir();
    const path = join(dir, "cetra", "credentials.toml");
    await saveToken(path, "http://a:1", "pat-cetra-abc");
    expect(await readFile(path, "utf8")).toBe('[servers."http://a:1"]\ntoken = "pat-cetra-abc"\n');
    expect(await loadCredentials(path)).toEqual(new Map([["http://a:1", "pat-cetra-abc"]]));
  });

  it("keeps other servers and writes them sorted by URL", async () => {
    const dir = await tempDir();
    const path = join(dir, "credentials.toml");
    await saveToken(path, "http://b:1", "pat-b");
    await saveToken(path, "http://a:1", "pat-a");
    const text = await readFile(path, "utf8");
    expect(text.indexOf('"http://a:1"')).toBeLessThan(text.indexOf('"http://b:1"'));
    expect(await loadCredentials(path)).toEqual(
      new Map([
        ["http://a:1", "pat-a"],
        ["http://b:1", "pat-b"],
      ]),
    );
  });

  it.skipIf(process.platform === "win32")(
    "writes the file 0600 and the directory 0700",
    async () => {
      const dir = await tempDir();
      const credentialsDir = join(dir, "cetra");
      await mkdir(credentialsDir, { mode: 0o755 });
      const path = join(credentialsDir, "credentials.toml");
      await saveToken(path, "http://a:1", "pat-a");
      expect((await stat(path)).mode & 0o777).toBe(0o600);
      expect((await stat(credentialsDir)).mode & 0o777).toBe(0o700);
      await expect(stat(`${path}.tmp`)).rejects.toThrow();
    },
  );

  it("refuses an unparsable file or a malformed servers table, without its content", async () => {
    const dir = await tempDir();
    const cases = [
      "not = [toml\n",
      "servers = 1\n",
      '[servers."http://h"]\ntoken = 1\n',
      '[servers."http://h"]\nname = "x"\n',
    ];
    for (const [index, text] of cases.entries()) {
      const path = join(dir, `case-${index}.toml`);
      await writeFile(path, text);
      const error = await loadCredentials(path).catch((caught: unknown) => caught);
      expect(error).toBeInstanceOf(CliError);
      expect((error as CliError).message).toBe("credentials file is invalid");
    }
  });
});
