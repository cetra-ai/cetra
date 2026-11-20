import { useSyncExternalStore } from "react";
import { STORAGE_PREFIX } from "@/product";

const OPEN_KEY = `${STORAGE_PREFIX}assistant.open`;
const SIZE_KEY = `${STORAGE_PREFIX}assistant.size`;
const CONVERSATION_KEY = `${STORAGE_PREFIX}assistant.conversation.`;

const DEFAULT_SIZE = 30;
const MIN_SIZE = 20;
const MAX_SIZE = 50;

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
  writeItem(OPEN_KEY, open ? "1" : "0");
  emit();
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

function clampSize(percent: number): number {
  return Math.min(MAX_SIZE, Math.max(MIN_SIZE, percent));
}

/** The panel's width in percent of the page area, between 20 and 50. */
export function readPanelSize(): number {
  const raw = readItem(SIZE_KEY);
  const stored = raw === null || raw.trim() === "" ? Number.NaN : Number(raw);
  if (!Number.isFinite(stored)) {
    return DEFAULT_SIZE;
  }
  return clampSize(stored);
}

export function writePanelSize(percent: number): void {
  writeItem(SIZE_KEY, String(clampSize(percent)));
}
