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

afterEach(() => {
  server.resetHandlers();
  cleanup();
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
