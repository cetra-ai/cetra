import type { DashboardDraft, EvaluatorDraft, RuleDraft, SqlDraft } from "@/lib/assistant/types";

type Handed =
  | { kind: "sql"; draft: SqlDraft }
  | { kind: "dashboard"; draft: DashboardDraft }
  | { kind: "evaluator"; draft: EvaluatorDraft }
  | { kind: "online_rule"; draft: RuleDraft };

type Kind = Handed["kind"];
type DraftOf<K extends Kind> = Extract<Handed, { kind: K }>["draft"];
type Slot = { [K in Kind]?: DraftOf<K> };

/** The drafts handed over and not yet taken, per project ref. Kept in memory only (D-AS-28). */
const held = new Map<string, Slot>();
const listeners = new Map<string, Set<() => void>>();

/** Hands a draft to the next target of its kind in project `ref`; replaces an untaken one of the same kind. */
export function handDraft(ref: string, handed: Handed): void {
  const slot: Slot = held.get(ref) ?? {};
  Object.assign(slot, { [handed.kind]: handed.draft });
  held.set(ref, slot);
  for (const listener of [...(listeners.get(ref) ?? [])]) {
    listener();
  }
}

/** The draft of `kind` handed in `ref`, without taking it, or `undefined`. */
export function peekDraft<K extends Kind>(ref: string, kind: K): DraftOf<K> | undefined {
  return held.get(ref)?.[kind];
}

/** Takes (and removes) the draft of `kind` handed in `ref`, or `undefined`. */
export function takeDraft<K extends Kind>(ref: string, kind: K): DraftOf<K> | undefined {
  const slot = held.get(ref);
  const draft = slot?.[kind];
  if (slot !== undefined && draft !== undefined) {
    delete slot[kind];
  }
  return draft;
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
