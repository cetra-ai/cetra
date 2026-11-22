import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { CliError, Exit } from "./errors.js";
import { problemMessage, requestJson } from "./http.js";
import { VERSION } from "./version.js";

interface Seen {
  method: string | undefined;
  headers: IncomingMessage["headers"];
  body: string;
}

let server: Server;
let base: string;
let last: Seen | undefined;

function readBody(request: IncomingMessage): Promise<string> {
  return new Promise((resolve) => {
    let text = "";
    request.setEncoding("utf8");
    request.on("data", (chunk: string) => {
      text += chunk;
    });
    request.on("end", () => resolve(text));
  });
}

function json(response: ServerResponse, status: number, body: unknown): void {
  response.writeHead(status, { "Content-Type": "application/json" });
  response.end(JSON.stringify(body));
}

beforeAll(async () => {
  server = createServer(async (request, response) => {
    const body = await readBody(request);
    last = { method: request.method, headers: request.headers, body };
    switch (request.url) {
      case "/echo":
        json(response, 200, { ok: true });
        return;
      case "/text":
        response.writeHead(200, { "Content-Type": "text/plain" });
        response.end("not json");
        return;
      case "/problem":
        json(response, 400, {
          type: "about:blank",
          title: "Bad Request",
          status: 400,
          code: "invalid_filter",
          detail: "SQL parse error",
          errors: [
            { field: "query", message: "unexpected token", position: { line: 1, column: 8 } },
          ],
        });
        return;
      case "/big":
        response.writeHead(500, { "Content-Type": "text/plain" });
        response.end("x".repeat(200 * 1024));
        return;
      case "/redirect":
        response.writeHead(302, { Location: "/echo" });
        response.end();
        return;
      case "/slow":
        // Never answers: the request must time out.
        return;
      default:
        response.writeHead(404);
        response.end();
    }
  });
  await new Promise<void>((resolve) => {
    server.listen(0, "127.0.0.1", resolve);
  });
  base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
});

afterAll(async () => {
  server.closeAllConnections();
  await new Promise<void>((resolve) => {
    server.close(() => resolve());
  });
});

describe("requestJson", () => {
  it("sends the default headers and no Origin, Cookie or Authorization", async () => {
    const answer = await requestJson("GET", `${base}/echo`);
    expect(answer.status).toBe(200);
    expect(answer.body).toEqual({ ok: true });
    expect(last?.headers.accept).toBe("application/json");
    expect(last?.headers["user-agent"]).toBe(`cetra-cli/${VERSION}`);
    expect(last?.headers.origin).toBeUndefined();
    expect(last?.headers.cookie).toBeUndefined();
    expect(last?.headers.authorization).toBeUndefined();
    expect(last?.headers["content-type"]).toBeUndefined();
  });

  it("sends Origin, Cookie, Bearer token and a JSON body when given", async () => {
    await requestJson("POST", `${base}/echo`, {
      body: { email: "a@example.com" },
      token: "pat-cetra-abc",
      cookie: "cetra_session=s1",
      origin: "http://localhost:8080",
    });
    expect(last?.method).toBe("POST");
    expect(last?.headers.origin).toBe("http://localhost:8080");
    expect(last?.headers.cookie).toBe("cetra_session=s1");
    expect(last?.headers.authorization).toBe("Bearer pat-cetra-abc");
    expect(last?.headers["content-type"]).toBe("application/json");
    expect(JSON.parse(last?.body ?? "")).toEqual({ email: "a@example.com" });
  });

  it("returns a non-JSON body as text with an undefined body", async () => {
    const answer = await requestJson("GET", `${base}/text`);
    expect(answer.status).toBe(200);
    expect(answer.body).toBeUndefined();
    expect(answer.text).toBe("not json");
  });

  it("returns a redirect as it is, without following it", async () => {
    const answer = await requestJson("GET", `${base}/redirect`);
    expect(answer.status).toBe(302);
    expect(answer.headers.get("location")).toBe("/echo");
    expect(last?.method).toBe("GET");
  });

  it("reads at most 64 KiB of an error body", async () => {
    const answer = await requestJson("GET", `${base}/big`);
    expect(answer.status).toBe(500);
    expect(answer.text.length).toBeLessThanOrEqual(64 * 1024);
    expect(answer.text.length).toBeGreaterThan(0);
    expect(answer.body).toBeUndefined();
  });

  it("throws Unreachable with the origin only when nothing listens", async () => {
    const closed = createServer();
    await new Promise<void>((resolve) => {
      closed.listen(0, "127.0.0.1", resolve);
    });
    const port = (closed.address() as AddressInfo).port;
    await new Promise<void>((resolve) => {
      closed.close(() => resolve());
    });
    const error = await requestJson("GET", `http://127.0.0.1:${port}/secret/path?token=1`).catch(
      (caught: unknown) => caught,
    );
    expect(error).toBeInstanceOf(CliError);
    expect((error as CliError).code).toBe(Exit.Unreachable);
    expect((error as CliError).message).toBe(`could not reach http://127.0.0.1:${port}`);
  });

  it("throws Unreachable when the server does not answer in time", async () => {
    const error = await requestJson("GET", `${base}/slow`, { timeoutMs: 50 }).catch(
      (caught: unknown) => caught,
    );
    expect(error).toBeInstanceOf(CliError);
    expect((error as CliError).code).toBe(Exit.Unreachable);
    expect((error as CliError).message).toBe(`could not reach ${base}`);
  });
});

describe("problemMessage", () => {
  it("renders the title, code, detail and field errors with positions", async () => {
    const answer = await requestJson("GET", `${base}/problem`);
    expect(problemMessage(answer.status, answer.body, base)).toBe(
      "Bad Request (invalid_filter): SQL parse error\n  query: unexpected token (line 1, column 8)",
    );
  });

  it("replaces control characters with spaces", () => {
    const body = {
      title: "Bad Request",
      code: "bad_request",
      detail: "bad\u001b[2Jthing",
    };
    expect(problemMessage(400, body, base)).toBe("Bad Request (bad_request): bad [2Jthing");
  });

  it("names the sign-in command for a 401", () => {
    expect(problemMessage(401, undefined, "http://h:1")).toBe(
      "the server rejected the token; run `cetra-cli login --url http://h:1`",
    );
  });

  it("gives the status line for a body that is not a problem", async () => {
    expect(problemMessage(502, "oops", base)).toBe("the server answered 502");
    expect(problemMessage(302, undefined, base)).toBe("the server answered 302");
  });
});
