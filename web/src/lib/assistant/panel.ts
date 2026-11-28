import { useSyncExternalStore } from "react";
import { track } from "@/lib/analytics";
import { STORAGE_PREFIX } from "@/product";

const OPEN_KEY = `${STORAGE_PREFIX}assistant.open`;
const CONVERSATION_KEY = `${STORAGE_PREFIX}assistant.conversation.`;
const WINDOW_KEY = `${STORAGE_PREFIX}assistant.window`;

const listeners = new Set<() => void>();
/**
 * Values kept for this page only, used when localStorage throws (private mode). `null` marks a
 * removed key. Only read when the storage read itself throws, so a later test or tab never sees
 * a stale value while storage works.
 */
const memory = new Map<string, string | null>();

function readItem(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return memory.get(key) ?? null;
  }
}

function writeItem(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
    // Private mode can throw. The choice still applies for this page.
    memory.set(key, value);
  }
}

function removeItem(key: string): void {
  try {
    localStorage.removeItem(key);
  } catch {
    memory.set(key, null);
  }
}

function emit(): void {
  for (const listener of listeners) {
    listener();
  }
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

function isOpen(): boolean {
  return readItem(OPEN_KEY) === "1";
}

export function setPanelOpen(open: boolean): void {
  const opening = open && !isOpen();
  writeItem(OPEN_KEY, open ? "1" : "0");
  emit();
  if (opening) {
    track({ name: "assistant.panel_opened" });
  }
}

export function togglePanel(): void {
  setPanelOpen(!isOpen());
}

/** Whether the assistant panel is open. Kept across projects and pages (D-AS-18). */
export function usePanelOpen(): boolean {
  return useSyncExternalStore(subscribe, isOpen, () => false);
}

function conversationKey(ref: string): string {
  return `${CONVERSATION_KEY}${ref}`;
}

/** The conversation shown for project `ref`, persisted per project. */
export function useCurrentConversation(ref: string): string | undefined {
  return useSyncExternalStore(
    subscribe,
    () => readItem(conversationKey(ref)) ?? undefined,
    () => undefined,
  );
}

export function setCurrentConversation(ref: string, id: string | undefined): void {
  if (id === undefined) {
    removeItem(conversationKey(ref));
  } else {
    writeItem(conversationKey(ref), id);
  }
  emit();
}

/** The floating window's position and size, in CSS pixels from the viewport's top left. */
export type WindowRect = { x: number; y: number; width: number; height: number };

type Viewport = { width: number; height: number };

export const WINDOW_MIN = { width: 320, height: 360 };
const WINDOW_DEFAULT = { width: 420, height: 600 };
/** The gap the default window keeps from the viewport's edges. */
const WINDOW_MARGIN = 16;

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

/**
 * `rect` fitted to the viewport: at least the minimum size, at most the viewport less a margin on
 * each side, and inside the viewport. A viewport smaller than the minimum keeps the minimum and
 * puts the window at its top left.
 */
export function fitWindow(rect: WindowRect, viewport: Viewport): WindowRect {
  const width = clamp(
    rect.width,
    WINDOW_MIN.width,
    Math.max(WINDOW_MIN.width, viewport.width - 2 * WINDOW_MARGIN),
  );
  const height = clamp(
    rect.height,
    WINDOW_MIN.height,
    Math.max(WINDOW_MIN.height, viewport.height - 2 * WINDOW_MARGIN),
  );
  return {
    x: clamp(rect.x, 0, Math.max(0, viewport.width - width)),
    y: clamp(rect.y, 0, Math.max(0, viewport.height - height)),
    width,
    height,
  };
}

/** The first window: the default size in the bottom right corner, fitted to the viewport. */
export function defaultWindow(viewport: Viewport): WindowRect {
  const { width, height } = fitWindow({ x: 0, y: 0, ...WINDOW_DEFAULT }, viewport);
  return fitWindow(
    {
      x: viewport.width - width - WINDOW_MARGIN,
      y: viewport.height - height - WINDOW_MARGIN,
      width,
      height,
    },
    viewport,
  );
}

function isRect(value: unknown): value is WindowRect {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const record = value as Record<string, unknown>;
  return ["x", "y", "width", "height"].every(
    (key) => typeof record[key] === "number" && Number.isFinite(record[key]),
  );
}

/** The saved window fitted to the viewport, or the default one when none is saved or it is bad. */
export function readWindowRect(viewport: Viewport): WindowRect {
  const raw = readItem(WINDOW_KEY);
  if (raw !== null) {
    try {
      const parsed: unknown = JSON.parse(raw);
      if (isRect(parsed)) {
        return fitWindow(parsed, viewport);
      }
    } catch {
      // A bad value falls back to the default window.
    }
  }
  return defaultWindow(viewport);
}

export function writeWindowRect(rect: WindowRect): void {
  writeItem(WINDOW_KEY, JSON.stringify(rect));
}
