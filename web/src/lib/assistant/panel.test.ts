import { afterEach, expect, it, vi } from "vitest";
import { startAnalytics, type WireEvent } from "@/lib/analytics";
import {
  defaultWindow,
  fitWindow,
  readWindowRect,
  setCurrentConversation,
  setPanelOpen,
  togglePanel,
  writeWindowRect,
} from "@/lib/assistant/panel";
import { STORAGE_PREFIX } from "@/product";

const OPEN_KEY = `${STORAGE_PREFIX}assistant.open`;
const CONVERSATION_ID = "01900000-0000-7000-8000-000000001a01";

afterEach(() => {
  vi.restoreAllMocks();
  localStorage.clear();
});

/** Starts analytics for one test; `stop` sends the queue and returns the sent events' names. */
function trackedNames(run: () => void): string[] {
  const sent: WireEvent[][] = [];
  const stop = startAnalytics({
    send: async (events) => {
      sent.push(events);
    },
    flushMs: 60_000,
  });
  try {
    run();
  } finally {
    stop();
  }
  return sent.flat().map((event) => event.name);
}

it("opening the panel is tracked once", () => {
  expect(
    trackedNames(() => {
      setPanelOpen(true);
      setPanelOpen(true);
      setPanelOpen(false);
    }),
  ).toEqual(["assistant.panel_opened"]);
});

it("toggling the panel open is tracked", () => {
  expect(trackedNames(() => togglePanel())).toEqual(["assistant.panel_opened"]);
});

it("persists the open state and the current conversation of a project", () => {
  setPanelOpen(true);
  expect(localStorage.getItem(OPEN_KEY)).toBe("1");
  togglePanel();
  expect(localStorage.getItem(OPEN_KEY)).toBe("0");
  togglePanel();
  expect(localStorage.getItem(OPEN_KEY)).toBe("1");

  setCurrentConversation("acme/support-bot", CONVERSATION_ID);
  expect(localStorage.getItem(`${STORAGE_PREFIX}assistant.conversation.acme/support-bot`)).toBe(
    CONVERSATION_ID,
  );
  setCurrentConversation("acme/support-bot", undefined);
  expect(
    localStorage.getItem(`${STORAGE_PREFIX}assistant.conversation.acme/support-bot`),
  ).toBeNull();
});

const WINDOW_KEY = `${STORAGE_PREFIX}assistant.window`;
const SCREEN = { width: 1440, height: 900 };

it("keeps the window in memory when storage throws", () => {
  vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
    throw new DOMException("denied", "SecurityError");
  });
  vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
    throw new DOMException("denied", "SecurityError");
  });
  writeWindowRect({ x: 10, y: 20, width: 400, height: 500 });
  expect(readWindowRect({ width: 1440, height: 900 })).toEqual({
    x: 10,
    y: 20,
    width: 400,
    height: 500,
  });
});

it("puts the first window in the bottom right corner", () => {
  expect(defaultWindow(SCREEN)).toEqual({ x: 1004, y: 284, width: 420, height: 600 });
});

it("shrinks the first window on a short screen", () => {
  expect(defaultWindow({ width: 1280, height: 500 })).toEqual({
    x: 844,
    y: 16,
    width: 420,
    height: 468,
  });
});

it("keeps a window inside the viewport", () => {
  expect(fitWindow({ x: -50, y: -10, width: 420, height: 600 }, SCREEN)).toEqual({
    x: 0,
    y: 0,
    width: 420,
    height: 600,
  });
  expect(fitWindow({ x: 1300, y: 800, width: 420, height: 600 }, SCREEN)).toEqual({
    x: 1020,
    y: 300,
    width: 420,
    height: 600,
  });
});

it("clamps a window's size between the minimum and the viewport", () => {
  expect(fitWindow({ x: 0, y: 0, width: 100, height: 100 }, SCREEN)).toEqual({
    x: 0,
    y: 0,
    width: 320,
    height: 360,
  });
  expect(fitWindow({ x: 0, y: 0, width: 5000, height: 5000 }, SCREEN)).toEqual({
    x: 0,
    y: 0,
    width: 1408,
    height: 868,
  });
  expect(fitWindow({ x: 40, y: 40, width: 420, height: 600 }, { width: 200, height: 200 })).toEqual(
    { x: 0, y: 0, width: 320, height: 360 },
  );
});

it("reads the saved window fitted to the viewport", () => {
  writeWindowRect({ x: 2000, y: 50, width: 500, height: 640 });
  expect(readWindowRect(SCREEN)).toEqual({ x: 940, y: 50, width: 500, height: 640 });
});

it("falls back to the default window for a bad saved value", () => {
  for (const bad of ["not json", "null", '{"x":1,"y":2,"width":"wide","height":3}', "[1,2,3,4]"]) {
    localStorage.setItem(WINDOW_KEY, bad);
    expect(readWindowRect(SCREEN)).toEqual(defaultWindow(SCREEN));
  }
});
