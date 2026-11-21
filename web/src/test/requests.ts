import { server } from "@/mocks/node";

/** One request a test made: its method, its path and, for requests with a JSON body, the body. */
type RecordedCall = { method: string; path: string; body?: unknown };

/**
 * Records the requests the page sends until `stop`. A body is read after the request starts, so
 * it may be missing from a call read at once; wait for it with `waitFor`.
 */
export function recordRequests() {
  const calls: RecordedCall[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const call: RecordedCall = { method: request.method, path: new URL(request.url).pathname };
    calls.push(call);
    if (request.method === "POST" || request.method === "PATCH" || request.method === "PUT") {
      void request
        .clone()
        .json()
        .then((body: unknown) => {
          call.body = body;
        })
        .catch(() => undefined);
    }
  };
  server.events.on("request:start", onStart);
  return {
    calls,
    stop() {
      server.events.removeListener("request:start", onStart);
    },
  };
}

/** Records the full URL of every request the page sends until `stop`. */
export function recordUrls(): { urls: URL[]; stop: () => void } {
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    urls.push(new URL(request.url));
  };
  server.events.on("request:start", onStart);
  return {
    urls,
    stop() {
      server.events.removeListener("request:start", onStart);
    },
  };
}
