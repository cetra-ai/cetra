import "@testing-library/jest-dom/vitest";
import { cleanup, configure } from "@testing-library/react";
import { HttpNetworkFrame } from "msw/experimental";
import { afterAll, afterEach, beforeAll, beforeEach } from "vitest";
import { resetListAnchors } from "@/lib/list-anchor";
import { dashboardHandlers, seedDashboards } from "../mocks/dashboards";
import { server } from "../mocks/node";
import { savedViewHandlers, seedSavedViews } from "../mocks/saved-views";
import { installTestEventSource } from "./event-source";
import { installLayout, restoreLayout } from "./layout";
import { installMatchMedia, resetMatchMedia } from "./match-media";
import { onlineRulePreviewHandler } from "./online-rule-preview";

// Under CPU load the first render of a file can take several seconds.
configure({ asyncUtilTimeout: 5_000 });

installTestEventSource();

const unhandled: string[] = [];

// Kept before a test can fake the timers: the teardown below waits on the real one.
const realSetTimeout = globalThis.setTimeout;

// Requests the app passed to `fetch`, and requests MSW has started to handle. A page can send a
// request when it unmounts (the reviewer releases its lease), and MSW starts it a few ticks after
// `cleanup`; the teardown waits until it has, or it would land in the next test's recorder.
let fetched = 0;
let started = 0;

/** Waits until MSW has started every request `fetch` was given, for at most 50 macrotasks. */
async function settleTeardownRequests(): Promise<void> {
  for (let tick = 0; tick < 50 && (tick === 0 || started < fetched); tick += 1) {
    await new Promise((resolve) => realSetTimeout(resolve, 0));
  }
  fetched = 0;
  started = 0;
}

beforeAll(() => {
  server.listen({
    onUnhandledFrame({ frame, defaults }) {
      if (frame instanceof HttpNetworkFrame) {
        const { request } = frame.data;
        unhandled.push(`${request.method} ${new URL(request.url).pathname}`);
      } else {
        unhandled.push("a non-HTTP frame");
      }
      defaults.error();
      // A callback only prints; on its own it lets the request through to the
      // real network. Throwing a network error rejects it, as "error" did.
      throw Response.error();
    },
  });
  // Wraps MSW's fetch, which `listen` installed; `close` puts the original back.
  const mswFetch = globalThis.fetch;
  globalThis.fetch = (...args: Parameters<typeof fetch>) => {
    fetched += 1;
    return mswFetch(...args);
  };
  server.events.on("request:start", () => {
    started += 1;
  });
});

installMatchMedia();
installLayout();

window.scrollTo = () => {};

if (!Element.prototype.hasPointerCapture) {
  Element.prototype.hasPointerCapture = () => false;
}
if (!Element.prototype.releasePointerCapture) {
  Element.prototype.releasePointerCapture = () => {};
}
if (!Element.prototype.scrollIntoView) {
  Element.prototype.scrollIntoView = () => {};
}
if (typeof globalThis.ResizeObserver === "undefined") {
  globalThis.ResizeObserver = class ResizeObserver {
    observe(): void {}
    unobserve(): void {}
    disconnect(): void {}
  };
}

// jsdom has no IntersectionObserver. Report every observed element as visible
// at once, so lazily mounted cards render as they do in a full page.
if (typeof globalThis.IntersectionObserver === "undefined") {
  globalThis.IntersectionObserver = class IntersectionObserverStub {
    constructor(private readonly callback: IntersectionObserverCallback) {}
    observe(target: Element): void {
      this.callback(
        [{ isIntersecting: true, target } as IntersectionObserverEntry],
        this as unknown as IntersectionObserver,
      );
    }
    unobserve(): void {}
    disconnect(): void {}
    takeRecords(): IntersectionObserverEntry[] {
      return [];
    }
  } as unknown as typeof IntersectionObserver;
}

beforeEach(() => {
  server.use(
    ...savedViewHandlers(seedSavedViews()),
    ...dashboardHandlers(seedDashboards()),
    onlineRulePreviewHandler(),
  );
});

afterEach(async () => {
  // Unmount first, while the test's handlers still answer what the unmount sends.
  cleanup();
  await settleTeardownRequests();
  server.resetHandlers();
  localStorage.clear();
  sessionStorage.clear();
  resetListAnchors();
  document.documentElement.className = "";
  document.documentElement.style.colorScheme = "";
  resetMatchMedia();
  const found = unhandled.splice(0);
  if (found.length > 0) {
    throw new Error(`Unhandled requests: ${found.join(", ")}`);
  }
});

afterAll(() => {
  restoreLayout();
  server.close();
});
