import { useEffect, useRef, useState } from "react";
import { reportUnauthorized } from "@/api/session";
import type { Schema } from "@/api/types";

const BACKOFF_START_MS = 1_000;
const BACKOFF_CAP_MS = 30_000;
const PING_GAP_MS = 45_000;

export type StreamStatus = "connecting" | "open" | "retrying" | "closed";

export type StreamEvent =
  | { type: "span"; span: Schema<"SpanSummary"> }
  | { type: "trace"; trace: Schema<"TraceSummary"> }
  | { type: "resync" };

/**
 * List stream URL. Same query as the trace list, without paging or time.
 * `project` is `projectRef(org, project)`.
 */
export function traceStreamUrl(
  project: string,
  query: {
    filter?: string;
    search?: string;
    includeEval?: boolean;
    includeExperiments?: boolean;
  },
): string {
  const params = new URLSearchParams();
  if (query.filter) {
    params.set("filter", query.filter);
  }
  if (query.search) {
    params.set("search", query.search);
  }
  if (query.includeEval) {
    params.set("include_eval", "true");
  }
  if (query.includeExperiments) {
    params.set("include_experiments", "true");
  }
  const path = `/api/v1/projects/${encodeURIComponent(project)}/traces/stream`;
  const search = params.toString();
  return search ? `${path}?${search}` : path;
}

/**
 * A status that ends the stream for good: 4xx other than 408 and 429. 401 is also
 * handled by the caller, which routes it to the session handler first.
 */
function stopsStream(status: number): boolean {
  return status >= 400 && status < 500 && status !== 408 && status !== 429;
}

export function openStream(
  url: string,
  onEvent: (event: StreamEvent) => void,
  onStatus: (status: StreamStatus) => void,
): () => void {
  let attempt = 0;
  let everOpened = false;
  let closed = false;
  let socket: EventSource | undefined;
  let retryTimer: ReturnType<typeof setTimeout> | undefined;
  let pingTimer: ReturnType<typeof setTimeout> | undefined;
  let probe: AbortController | undefined;
  const dropped = { count: 0 };

  const clearPing = () => {
    if (pingTimer !== undefined) {
      clearTimeout(pingTimer);
      pingTimer = undefined;
    }
  };

  const armPing = () => {
    clearPing();
    pingTimer = setTimeout(() => {
      fail();
    }, PING_GAP_MS);
  };

  /** Ends this call for good: no retry, no probe, the EventSource closed. */
  const finish = () => {
    closed = true;
    if (retryTimer !== undefined) {
      clearTimeout(retryTimer);
      retryTimer = undefined;
    }
    clearPing();
    probe?.abort();
    probe = undefined;
    const current = socket;
    socket = undefined;
    current?.close();
    onStatus("closed");
  };

  const scheduleRetry = () => {
    onStatus("retrying");
    const delay = Math.min(BACKOFF_START_MS * 2 ** attempt, BACKOFF_CAP_MS);
    attempt += 1;
    retryTimer = setTimeout(connect, delay);
  };

  /** A drop after the stream opened: close it and retry with backoff. */
  const fail = () => {
    if (closed || socket === undefined) {
      return;
    }
    const current = socket;
    socket = undefined;
    current.close();
    clearPing();
    scheduleRetry();
  };

  /**
   * The stream failed before it opened, so the status is unknown. One plain request
   * tells whether to retry: 401 goes to the session handler and stops, other 4xx stop,
   * anything else retries.
   */
  const checkBeforeOpen = (controller: AbortController) => {
    const apiPath = new URL(url, location.origin).pathname;
    fetch(url, {
      headers: { Accept: "text/event-stream" },
      credentials: "same-origin",
      cache: "no-store",
      signal: controller.signal,
    }).then(
      (response) => {
        if (probe !== controller) {
          return;
        }
        probe = undefined;
        // A 200 answers with an endless event stream. Its headers are all we need.
        controller.abort();
        if (response.status === 401) {
          reportUnauthorized(apiPath);
          finish();
        } else if (stopsStream(response.status)) {
          finish();
        } else {
          scheduleRetry();
        }
      },
      () => {
        if (probe !== controller) {
          return;
        }
        probe = undefined;
        scheduleRetry();
      },
    );
  };

  const connect = () => {
    if (closed) {
      return;
    }
    retryTimer = undefined;
    onStatus("connecting");
    const source = new EventSource(url, { withCredentials: false });
    socket = source;
    let opened = false;

    source.addEventListener("open", () => {
      if (socket !== source) {
        return;
      }
      opened = true;
      onStatus("open");
      if (everOpened) {
        onEvent({ type: "resync" });
      }
      everOpened = true;
      attempt = 0;
      armPing();
    });

    source.addEventListener("error", () => {
      if (socket !== source) {
        return;
      }
      if (opened) {
        fail();
        return;
      }
      socket = undefined;
      source.close();
      clearPing();
      const controller = new AbortController();
      probe = controller;
      checkBeforeOpen(controller);
    });

    source.addEventListener("ping", () => {
      if (socket === source) {
        armPing();
      }
    });

    source.addEventListener("span", (event) => {
      deliver(source, "span", event);
    });
    source.addEventListener("trace", (event) => {
      deliver(source, "trace", event);
    });
    source.addEventListener("resync", (event) => {
      deliver(source, "resync", event);
    });
  };

  const deliver = (source: EventSource, type: StreamEvent["type"], event: Event) => {
    if (socket !== source) {
      return;
    }
    const value = parseObject(event);
    if (!value) {
      dropped.count += 1;
      return;
    }
    if (type === "span") {
      onEvent({ type, span: value as Schema<"SpanSummary"> });
    } else if (type === "trace") {
      onEvent({ type, trace: value as Schema<"TraceSummary"> });
    } else {
      onEvent({ type: "resync" });
    }
  };

  connect();

  return finish;
}

export function useStream(
  url: string | undefined,
  onEvent: (event: StreamEvent) => void,
): { status: StreamStatus } {
  const [status, setStatus] = useState<StreamStatus>(url ? "connecting" : "closed");
  const onEventRef = useRef(onEvent);
  onEventRef.current = onEvent;

  useEffect(() => {
    if (!url) {
      setStatus("closed");
      return;
    }
    let active = true;
    setStatus("connecting");
    const stop = openStream(
      url,
      (event) => {
        onEventRef.current(event);
      },
      (next) => {
        if (active) {
          setStatus(next);
        }
      },
    );
    return () => {
      active = false;
      stop();
    };
  }, [url]);

  return { status };
}

function parseObject(event: Event): Record<string, unknown> | undefined {
  if (!(event instanceof MessageEvent) || typeof event.data !== "string") {
    return undefined;
  }
  try {
    const value: unknown = JSON.parse(event.data);
    if (value === null || typeof value !== "object" || Array.isArray(value)) {
      return undefined;
    }
    return value as Record<string, unknown>;
  } catch {
    return undefined;
  }
}
