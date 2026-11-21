import { type ChildProcess, type SpawnOptions, spawn } from "node:child_process";
import { afterEach, describe, expect, it } from "vitest";
import { openBrowser } from "./browser.js";
import {
  deviceSignIn,
  type Grant,
  isPersonalToken,
  isSecretKey,
  isUserCode,
  type Purpose,
  step,
} from "./device.js";
import { CliError, Exit } from "./errors.js";
import { type CannedAnswer, type CannedServer, startCanned } from "./test-server.js";

const PAT = `pat-cetra-${"A".repeat(32)}`;
const KEY = `sk-cetra-${"B".repeat(32)}`;
const DEVICE_CODE = "d".repeat(43);
const SERVER = "http://127.0.0.1:8080";
const PROBLEM = (code: string) => ({ status: 400, body: { code } });
const GRANT = { server: SERVER, token: PAT };
const INIT_PROJECT = {
  id: "0190a0b0-0000-7000-8000-000000000001",
  org_slug: "acme",
  slug: "demo",
  name: "Demo",
};

/** A code answer body with a 1-second interval and a 600-second lifetime. */
const CODE_BODY = {
  device_code: DEVICE_CODE,
  user_code: "BCDF-GHJK",
  verification_uri: "http://127.0.0.1:9/device",
  verification_uri_complete: "http://127.0.0.1:9/device?code=BCDF-GHJK",
  expires_in: 600,
  interval: 1,
};
const CODE: CannedAnswer = { status: 200, body: CODE_BODY };

const servers: CannedServer[] = [];

afterEach(async () => {
  for (const server of servers.splice(0)) await server.close();
});

async function canned(answers: CannedAnswer[]): Promise<CannedServer> {
  const server = await startCanned(answers);
  servers.push(server);
  return server;
}

/** The error a failed sign-in throws; fails the test when it does not throw. */
async function failureOf(promise: Promise<unknown>): Promise<CliError> {
  try {
    await promise;
  } catch (error) {
    if (error instanceof CliError) return error;
    throw error;
  }
  throw new Error("expected the sign-in to fail");
}

/** Sleeps do not wait, and the clock is fixed unless a test moves it. */
function fast(sleeps: number[] = [], now: () => number = () => 0) {
  return { sleep: async (ms: number) => void sleeps.push(ms), now };
}

function signIn(
  server: string,
  purpose: Purpose,
  lines: string[],
  deps: ReturnType<typeof fast>,
  project?: string,
): Promise<Grant> {
  return deviceSignIn({
    server,
    purpose,
    noBrowser: true,
    project,
    stderr: (line) => lines.push(line),
    deps,
  });
}

describe("step", () => {
  it("maps every poll answer to its meaning", () => {
    expect(step(400, { code: "authorization_pending" })).toBe("wait");
    expect(step(400, { code: "slow_down" })).toBe("slow_down");
    expect(step(400, { code: "access_denied" })).toBe("denied");
    expect(step(400, { code: "expired_token" })).toBe("expired");
    expect(step(400, { code: "something_else" })).toEqual({ failed: 400 });
    expect(step(400, undefined)).toEqual({ failed: 400 });
    for (const status of [429, 502, 503, 504]) {
      expect(step(status, undefined)).toBe("wait");
    }
    expect(step(500, { code: "authorization_pending" })).toEqual({ failed: 500 });
    expect(step(401, undefined)).toEqual({ failed: 401 });
  });
});

describe("the shape checks", () => {
  it("accepts a user code only in XXXX-XXXX capitals", () => {
    expect(isUserCode("BCDF-GHJK")).toBe(true);
    expect(isUserCode("bcdf-ghjk")).toBe(false);
    expect(isUserCode("BCDFGHJK")).toBe(false);
    expect(isUserCode("BCDF-GHJ")).toBe(false);
    expect(isUserCode("BCDF-GHJKL")).toBe(false);
  });

  it("accepts a personal token of pat-cetra- and 32 letters or digits", () => {
    expect(isPersonalToken(PAT)).toBe(true);
    expect(isPersonalToken(`pat-cetra-${"A".repeat(31)}`)).toBe(false);
    expect(isPersonalToken(`pat-cetra-${"A".repeat(33)}`)).toBe(false);
    expect(isPersonalToken(`pat-other-${"A".repeat(32)}`)).toBe(false);
    expect(isPersonalToken(`pat-cetra-${"A".repeat(31)}-`)).toBe(false);
  });

  it("accepts a secret key of sk-cetra- and 32 letters or digits", () => {
    expect(isSecretKey(KEY)).toBe(true);
    expect(isSecretKey(`sk-cetra-${"B".repeat(31)}`)).toBe(false);
    expect(isSecretKey(`sk-other-${"B".repeat(32)}`)).toBe(false);
  });
});

describe("deviceSignIn", () => {
  it("prints the URL and the code, polls past a pending answer, and returns the grant", async () => {
    const server = await canned([
      CODE,
      PROBLEM("authorization_pending"),
      { status: 200, body: GRANT },
    ]);
    const lines: string[] = [];
    const grant = await signIn(server.url, "login", lines, fast());

    expect(grant).toEqual({ server: SERVER, token: PAT });
    expect(lines).toEqual([
      "Open http://127.0.0.1:9/device?code=BCDF-GHJK in your browser and approve the code.",
      "Code: BCDF-GHJK",
    ]);
    expect(server.requests.map((request) => request.path)).toEqual([
      "/api/auth/device/code",
      "/api/auth/device/token",
      "/api/auth/device/token",
    ]);
    for (const request of server.requests) {
      expect(request.headers.origin).toBeUndefined();
      expect(request.headers.authorization).toBeUndefined();
    }
    expect(server.requests[0]?.body).toMatchObject({ purpose: "login" });
    expect(server.requests[1]?.body).toEqual({ device_code: DEVICE_CODE });
  });

  it("grows the interval on slow_down, which shows in the sleeps", async () => {
    const server = await canned([CODE, PROBLEM("slow_down"), { status: 200, body: GRANT }]);
    const sleeps: number[] = [];
    await signIn(server.url, "login", [], fast(sleeps));
    expect(sleeps).toEqual([1000, 6000]);
  });

  it("throws the denial message on access_denied", async () => {
    const server = await canned([CODE, PROBLEM("access_denied")]);
    const error = await failureOf(signIn(server.url, "login", [], fast()));
    expect(error).toMatchObject({ message: "sign-in denied in the browser", code: Exit.SignIn });
  });

  it("throws the expiry message on expired_token", async () => {
    const server = await canned([CODE, PROBLEM("expired_token")]);
    const error = await failureOf(signIn(server.url, "login", [], fast()));
    expect(error).toMatchObject({
      message: "the sign-in code expired; run the command again",
      code: Exit.SignIn,
    });
  });

  it("throws with the status on a server error", async () => {
    const server = await canned([CODE, { status: 500, body: {} }]);
    const error = await failureOf(signIn(server.url, "login", [], fast()));
    expect(error).toMatchObject({ message: "sign-in failed (HTTP 500)", code: Exit.SignIn });
  });

  it("refuses a code the server does not issue with 200", async () => {
    const server = await canned([{ status: 500, body: {} }]);
    const error = await failureOf(signIn(server.url, "login", [], fast()));
    expect(error).toMatchObject({
      message: "the server refused a sign-in code (HTTP 500)",
      code: Exit.SignIn,
    });
  });

  it("throws the expiry message once the deadline passes, without polling again", async () => {
    // expires_in 1 gives a deadline of 1 s + 5 s slack; the sleep moves the clock past it.
    const server = await canned([{ status: 200, body: { ...CODE_BODY, expires_in: 1 } }]);
    let clock = 0;
    const error = await failureOf(
      signIn(server.url, "login", [], {
        sleep: async () => {
          clock += 10_000;
        },
        now: () => clock,
      }),
    );
    expect(error).toMatchObject({
      message: "the sign-in code expired; run the command again",
      code: Exit.SignIn,
    });
    expect(server.requests).toHaveLength(1);
  });

  it("refuses a javascript: sign-in URL before anything is printed", async () => {
    const server = await canned([
      {
        status: 200,
        body: { ...CODE_BODY, verification_uri_complete: "javascript:alert(1)" },
      },
    ]);
    const lines: string[] = [];
    const error = await failureOf(signIn(server.url, "login", lines, fast()));
    expect(error).toMatchObject({
      message: "the server sent an invalid sign-in URL",
      code: Exit.SignIn,
    });
    expect(lines).toEqual([]);
    expect(server.requests).toHaveLength(1);
  });

  it("refuses a user code that is not XXXX-XXXX before anything is printed", async () => {
    const server = await canned([{ status: 200, body: { ...CODE_BODY, user_code: "abcd-efgh" } }]);
    const lines: string[] = [];
    const error = await failureOf(signIn(server.url, "login", lines, fast()));
    expect(error).toMatchObject({
      message: "the server sent an invalid sign-in code",
      code: Exit.SignIn,
    });
    expect(lines).toEqual([]);
  });

  it("adds the project to the sign-in URL's query", async () => {
    const server = await canned([CODE, { status: 200, body: GRANT }]);
    const lines: string[] = [];
    await signIn(server.url, "login", lines, fast(), "acme/demo");
    expect(lines[0]).toBe(
      "Open http://127.0.0.1:9/device?code=BCDF-GHJK&project=acme%2Fdemo in your browser and approve the code.",
    );
  });

  it("returns an init grant with its project and ingest key", async () => {
    const server = await canned([
      CODE,
      { status: 200, body: { ...GRANT, project: INIT_PROJECT, ingest_key: KEY } },
    ]);
    const grant = await signIn(server.url, "init", [], fast(), "acme/demo");
    expect(grant).toEqual({ server: SERVER, token: PAT, project: INIT_PROJECT, ingest_key: KEY });
    expect(server.requests[0]?.body).toMatchObject({ purpose: "init" });
  });

  it("throws on an init grant without an ingest key", async () => {
    const server = await canned([CODE, { status: 200, body: { ...GRANT, project: INIT_PROJECT } }]);
    const error = await failureOf(signIn(server.url, "init", [], fast()));
    expect(error).toMatchObject({
      message: "the server returned an invalid grant",
      code: Exit.SignIn,
    });
  });

  it("throws on a grant whose token is not a personal token", async () => {
    const server = await canned([CODE, { status: 200, body: { server: SERVER, token: "nope" } }]);
    const error = await failureOf(signIn(server.url, "login", [], fast()));
    expect(error).toMatchObject({
      message: "the server returned an invalid grant",
      code: Exit.SignIn,
    });
  });
});

describe("openBrowser", () => {
  /** A spawn stand-in that records what it was asked to run. */
  function recorder() {
    const calls: { command: string; args: readonly string[] }[] = [];
    const fake = ((command: string, args: readonly string[]) => {
      calls.push({ command, args });
      return { on: () => undefined, unref: () => undefined } as unknown as ChildProcess;
    }) as unknown as typeof spawn;
    return { calls, fake };
  }

  it("starts nothing on Linux without a display", () => {
    const { calls, fake } = recorder();
    openBrowser("http://127.0.0.1:9/device", { spawn: fake }, {}, "linux");
    expect(calls).toEqual([]);
  });

  it("runs the opener of each platform with the URL as one argument", () => {
    const url = "http://127.0.0.1:9/device?code=BCDF-GHJK&project=acme%2Fdemo";
    let recorded = recorder();
    openBrowser(url, { spawn: recorded.fake }, { DISPLAY: ":0" }, "linux");
    expect(recorded.calls).toEqual([{ command: "xdg-open", args: [url] }]);

    recorded = recorder();
    openBrowser(url, { spawn: recorded.fake }, {}, "darwin");
    expect(recorded.calls).toEqual([{ command: "open", args: [url] }]);

    recorded = recorder();
    openBrowser(url, { spawn: recorded.fake }, {}, "win32");
    expect(recorded.calls).toEqual([
      { command: "rundll32", args: ["url.dll,FileProtocolHandler", url] },
    ]);
  });

  it("does not throw when the opener program is missing", async () => {
    // A real spawn of a program that does not exist: the error listener keeps the process up.
    const missing = ((_command: string, args: string[], options: SpawnOptions) =>
      spawn("cetra-no-such-opener", args, options)) as unknown as typeof spawn;
    expect(() =>
      openBrowser("http://127.0.0.1:9/device", { spawn: missing }, { DISPLAY: ":0" }, "linux"),
    ).not.toThrow();
    await new Promise((resolve) => setTimeout(resolve, 200));
  });
});
