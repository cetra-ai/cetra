/**
 * Product analytics (track TL M6, D-TL-14). Catalog events go to this server only
 * (`POST /api/v1/analytics/events`), never to a third party; nothing is sent until
 * `startAnalytics` runs, which `installAnalytics` does only when `GET /api/auth/config` says
 * `analytics: true`. The catalog is `fixtures/analytics/catalog.json`.
 */

/** The web events of the catalog, with their properties. */
export type WebEvent =
  | { name: "page.viewed"; properties: { route: string } }
  | { name: "command_palette.opened" }
  | { name: "live_tail.toggled"; properties: { on: boolean } }
  | { name: "list_layout.changed"; properties: { layout: "drawer" | "bottom" | "page" } }
  | { name: "theme.changed"; properties: { theme: "light" | "dark" | "system" } }
  | { name: "assistant.panel_opened" };

/** The property keys of each web event, sorted (checked against the golden catalog). */
export const WEB_EVENT_PROPERTIES: Record<WebEvent["name"], readonly string[]> = {
  "assistant.panel_opened": [],
  "command_palette.opened": [],
  "list_layout.changed": ["layout"],
  "live_tail.toggled": ["on"],
  "page.viewed": ["route"],
  "theme.changed": ["theme"],
};

/** One event as sent: `org` and `project` are the page's route parameters. */
export interface WireEvent {
  name: WebEvent["name"];
  org?: string;
  project?: string;
  properties?: Record<string, string | boolean>;
}

/** The page an event happened on. */
export interface AnalyticsContext {
  org?: string;
  project?: string;
}

export type Sender = (events: WireEvent[]) => Promise<void>;

/** Events per request; the server reads at most 50. A full batch is sent at once, so no more
 * than 50 events ever wait. */
export const MAX_BATCH = 50;
/** A batch goes out at least this often while events wait. */
export const FLUSH_MS = 5_000;

export const ANALYTICS_PATH = "/api/v1/analytics/events";

interface Active {
  queue: WireEvent[];
  send: Sender;
  context: () => AnalyticsContext;
  timer: ReturnType<typeof setInterval>;
  onVisibility: () => void;
}

let active: Active | undefined;

/**
 * Posts a batch to this server with the session cookie. Plain `fetch`, not the API client:
 * a 401 here must not send the user to the login page (`isSessionApiPath`). `keepalive`
 * lets the last batch leave while the page unloads.
 */
export const postEvents: Sender = async (events) => {
  await globalThis.fetch(new URL(ANALYTICS_PATH, window.location.origin).toString(), {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ events }),
    keepalive: true,
  });
};

/** Queues `event` with the current page's org and project. Does nothing while stopped. */
export function track(event: WebEvent): void {
  if (!active) {
    return;
  }
  const { org, project } = active.context();
  active.queue.push({
    name: event.name,
    ...(org === undefined ? {} : { org }),
    ...(project === undefined ? {} : { project }),
    ...("properties" in event ? { properties: event.properties } : {}),
  });
  if (active.queue.length >= MAX_BATCH) {
    flushAnalytics();
  }
}

/** Sends up to `MAX_BATCH` queued events. A failed send is dropped (no retry). */
export function flushAnalytics(): void {
  if (!active || active.queue.length === 0) {
    return;
  }
  const batch = active.queue.splice(0, MAX_BATCH);
  void active.send(batch).catch(() => undefined);
}

/**
 * Starts sending: a batch every `flushMs` and when the page is hidden. Returns `stop`, which
 * sends everything still queued and stops tracking; a `stop` of a start that a later start
 * replaced does nothing. A second start replaces the first.
 */
export function startAnalytics(options: {
  send?: Sender;
  context?: () => AnalyticsContext;
  flushMs?: number;
}): () => void {
  stopActive();
  const onVisibility = () => {
    if (document.visibilityState === "hidden") {
      flushAnalytics();
    }
  };
  const instance: Active = {
    queue: [],
    send: options.send ?? postEvents,
    context: options.context ?? (() => ({})),
    timer: setInterval(flushAnalytics, options.flushMs ?? FLUSH_MS),
    onVisibility,
  };
  active = instance;
  document.addEventListener("visibilitychange", onVisibility);
  return () => {
    if (active === instance) {
      stopActive();
    }
  };
}

function stopActive(): void {
  if (!active) {
    return;
  }
  while (active.queue.length > 0) {
    flushAnalytics();
  }
  clearInterval(active.timer);
  document.removeEventListener("visibilitychange", active.onVisibility);
  active = undefined;
}
