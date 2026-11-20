import { afterEach, expect, it, vi } from "vitest";
import {
  readPanelSize,
  setCurrentConversation,
  setPanelOpen,
  togglePanel,
  writePanelSize,
} from "@/lib/assistant/panel";
import { STORAGE_PREFIX } from "@/product";

const OPEN_KEY = `${STORAGE_PREFIX}assistant.open`;
const SIZE_KEY = `${STORAGE_PREFIX}assistant.size`;
const CONVERSATION_ID = "01900000-0000-7000-8000-000000001a01";

afterEach(() => {
  vi.restoreAllMocks();
  localStorage.clear();
});

it("clamps the panel size to 20 to 50 percent and defaults to 30", () => {
  expect(readPanelSize()).toBe(30);
  writePanelSize(80);
  expect(readPanelSize()).toBe(50);
  writePanelSize(5);
  expect(readPanelSize()).toBe(20);
  writePanelSize(42);
  expect(readPanelSize()).toBe(42);
  expect(localStorage.getItem(SIZE_KEY)).toBe("42");
});

it("uses the default size when the stored size is not a number", () => {
  localStorage.setItem(SIZE_KEY, "wide");
  expect(readPanelSize()).toBe(30);
  localStorage.setItem(SIZE_KEY, "");
  expect(readPanelSize()).toBe(30);
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

it("keeps the size in memory when storage throws", () => {
  vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
    throw new DOMException("denied", "SecurityError");
  });
  vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
    throw new DOMException("denied", "SecurityError");
  });
  writePanelSize(40);
  expect(readPanelSize()).toBe(40);
});
