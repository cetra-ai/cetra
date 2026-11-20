import type { DashboardDraft, EvaluatorDraft, RuleDraft, SqlDraft } from "@/lib/assistant/types";

type Handed =
  | { kind: "sql"; draft: SqlDraft }
  | { kind: "dashboard"; draft: DashboardDraft }
  | { kind: "evaluator"; draft: EvaluatorDraft }
  | { kind: "online_rule"; draft: RuleDraft };

type Kind = Handed["kind"];
type DraftOf<K extends Kind> = Extract<Handed, { kind: K }>["draft"];
type Slot = { [K in Kind]?: { draft: DraftOf<K>; handedAt: number } };

/** A draft that is not taken within this long is dropped: it was for a page the user left. */
export const DRAFT_TTL_MS = 60_000;

/** The drafts handed over and not yet taken, per project ref. Kept in memory only (D-AS-28). */
const held = new Map<string, Slot>();
const listeners = new Map<string, Set<() => void>>();

/** Hands a draft to the next target of its kind in project `ref`; replaces an untaken one of the same kind. */
export function handDraft(ref: string, handed: Handed): void {
  const slot: Slot = held.get(ref) ?? {};
  Object.assign(slot, { [handed.kind]: { draft: handed.draft, handedAt: Date.now() } });
  held.set(ref, slot);
  for (const listener of [...(listeners.get(ref) ?? [])]) {
    listener();
  }
}

/** The held entry of `kind` in `ref`; an expired one is deleted and reads as absent. */
function entryOf<K extends Kind>(ref: string, kind: K): Slot[K] {
  const slot = held.get(ref);
  const entry = slot?.[kind];
  if (slot === undefined || entry === undefined) {
    return undefined;
  }
  if (Date.now() - entry.handedAt > DRAFT_TTL_MS) {
    delete slot[kind];
    return undefined;
  }
  return entry;
}

/** The draft of `kind` handed in `ref`, without taking it, or `undefined`. */
export function peekDraft<K extends Kind>(ref: string, kind: K): DraftOf<K> | undefined {
  return entryOf(ref, kind)?.draft;
}

/** Takes (and removes) the draft of `kind` handed in `ref`, or `undefined`. */
export function takeDraft<K extends Kind>(ref: string, kind: K): DraftOf<K> | undefined {
  const entry = entryOf(ref, kind);
  if (entry !== undefined) {
    delete held.get(ref)?.[kind];
  }
  return entry?.draft;
}

/** Calls `listener` after each `handDraft` in `ref`; returns the unsubscribe function. */
export function subscribeDrafts(ref: string, listener: () => void): () => void {
  const set = listeners.get(ref) ?? new Set<() => void>();
  set.add(listener);
  listeners.set(ref, set);
  return () => {
    set.delete(listener);
  };
}
