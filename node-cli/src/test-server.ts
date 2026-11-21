import { createServer } from "node:http";

/** One canned answer: the status, a JSON body and extra headers. */
export interface CannedAnswer {
  status: number;
  body: unknown;
  headers?: Record<string, string>;
}

/** One request the canned server received. Header names are lower case. */
export interface RecordedRequest {
  method: string;
  path: string;
  headers: Record<string, string>;
  body: unknown;
}

export interface CannedServer {
  url: string;
  requests: RecordedRequest[];
  close(): Promise<void>;
}

/** A local HTTP server for tests. It answers the requests in order with `answers`, records each
 *  request, and answers 500 once the list is used up. Test helper only, not in the build. */
export function startCanned(answers: CannedAnswer[]): Promise<CannedServer> {
  const requests: RecordedRequest[] = [];
  let next = 0;
  const server = createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (chunk: Buffer) => chunks.push(chunk));
    req.on("end", () => {
      const text = Buffer.concat(chunks).toString("utf8");
      let body: unknown;
      if (text !== "") {
        try {
          body = JSON.parse(text) as unknown;
        } catch {
          body = text;
        }
      }
      const headers: Record<string, string> = {};
      for (const [name, value] of Object.entries(req.headers)) {
        if (value !== undefined) headers[name] = Array.isArray(value) ? value.join(", ") : value;
      }
      requests.push({ method: req.method ?? "", path: req.url ?? "", headers, body });
      const answer = answers[next] ?? { status: 500, body: { title: "no canned answer" } };
      next += 1;
      res.writeHead(answer.status, { "content-type": "application/json", ...answer.headers });
      res.end(answer.body === undefined ? "" : JSON.stringify(answer.body));
    });
  });
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (address === null || typeof address === "string") {
        reject(new Error("the canned server has no port"));
        return;
      }
      resolve({
        url: `http://127.0.0.1:${address.port}`,
        requests,
        close: () =>
          new Promise<void>((done) => {
            server.closeAllConnections();
            server.close(() => done());
          }),
      });
    });
  });
}
