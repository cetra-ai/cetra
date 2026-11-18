import { useEffect, useRef, useState } from "react";
import type { Layout } from "react-grid-layout";
import { isStaleDashboard } from "@/api/dashboards";
import { problemMessage } from "@/api/problem";
import { withLayouts } from "@/lib/dashboards/draft";
import type { Definition } from "@/lib/dashboards/types";

/** Quiet time after the last finished move before the layout is saved. */
export const AUTOSAVE_DELAY_MS = 800;
/** How long the "Layout saved" bar offers Undo. */
export const UNDO_MS = 10_000;

export type LayoutAutosave = {
  /** The moved definition while it waits for or is being saved; undefined otherwise. */
  pending: Definition | undefined;
  /** A finished drag or resize (react-grid-layout's onDragStop / onResizeStop layout). */
  onMoveEnd: (layout: Layout) => void;
  /** Set for UNDO_MS after a successful layout save. */
  undo: { run: () => void } | null;
  /** A save (or an undo) is in flight. */
  saving: boolean;
};

type Args = {
  /** The saved definition on screen. */
  definition: Definition;
  /** The version it was read at; undefined when nothing is stored yet (the preset Overview, OE3). */
  version: number | undefined;
  /** A member (or a demo-org viewer on a browser dashboard), not in Edit, not narrow. */
  enabled: boolean;
  /** Saves a definition with the version read; resolves once the query cache holds the saved version. */
  persist: (definition: Definition, version: number | undefined) => Promise<unknown>;
  /** A 409: the move is dropped; reload. */
  onConflict: () => void;
  /** Another failure: the move is dropped; show the message. */
  onError: (message: string) => void;
};

type Timer = ReturnType<typeof setTimeout> | undefined;

/**
 * Saves the layout of a dashboard the user can edit, as soon as a drag or resize has settled. The
 * save names the version it was read at, so a change made elsewhere is a 409, not an overwrite.
 *
 * Every timer and callback reads the latest values from refs, so none of them sees a stale render.
 */
export function useLayoutAutosave({
  definition,
  version,
  enabled,
  persist,
  onConflict,
  onError,
}: Args): LayoutAutosave {
  const [pending, setPendingState] = useState<Definition | undefined>(undefined);
  const [undo, setUndoState] = useState<{ run: () => void } | null>(null);
  const [saving, setSavingState] = useState(false);

  const latest = useRef({ definition, version, persist, onConflict, onError });
  latest.current = { definition, version, persist, onConflict, onError };

  const pendingRef = useRef<Definition | undefined>(undefined);
  /** The saved definition from before the first of the pending moves; Undo restores it. */
  const beforeRef = useRef<Definition | undefined>(undefined);
  /** Whether the pending save offers Undo. An undo's own save does not. */
  const undoableRef = useRef(true);
  const savingRef = useRef(false);
  /** A flush was asked while a save ran: the save that finishes saves the pending move at once. */
  const flushRef = useRef(false);
  const delayTimer = useRef<Timer>(undefined);
  const undoTimer = useRef<Timer>(undefined);

  function setPending(next: Definition | undefined) {
    pendingRef.current = next;
    setPendingState(next);
  }

  function setSaving(next: boolean) {
    savingRef.current = next;
    setSavingState(next);
  }

  function clearUndo() {
    if (undoTimer.current !== undefined) {
      clearTimeout(undoTimer.current);
      undoTimer.current = undefined;
    }
    setUndoState(null);
  }

  /** Saves the pending move after the quiet time. A save that is running saves it when it ends. */
  function armDelay() {
    if (delayTimer.current !== undefined) {
      clearTimeout(delayTimer.current);
    }
    delayTimer.current = setTimeout(() => {
      delayTimer.current = undefined;
      drain();
    }, AUTOSAVE_DELAY_MS);
  }

  /** Saves the pending move now, unless a save is already running. */
  function drain() {
    const next = pendingRef.current;
    if (next === undefined || savingRef.current) {
      return;
    }
    if (delayTimer.current !== undefined) {
      clearTimeout(delayTimer.current);
      delayTimer.current = undefined;
    }
    void run(next);
  }

  async function run(next: Definition) {
    setSaving(true);
    try {
      await latest.current.persist(next, latest.current.version);
      // A move made during the save stays pending: it is saved next, with the version the grid
      // then holds.
      if (pendingRef.current === next) {
        setPending(undefined);
        if (undoableRef.current) {
          offerUndo();
        }
      }
    } catch (error) {
      setPending(undefined);
      clearUndo();
      if (isStaleDashboard(error)) {
        latest.current.onConflict();
      } else {
        latest.current.onError(problemMessage(error, "Could not save the layout."));
      }
    } finally {
      setSaving(false);
    }
    if (pendingRef.current !== undefined) {
      if (flushRef.current) {
        flushRef.current = false;
        drain();
      } else {
        armDelay();
      }
    }
    flushRef.current = false;
  }

  function offerUndo() {
    if (undoTimer.current !== undefined) {
      clearTimeout(undoTimer.current);
    }
    setUndoState({ run: undoSave });
    undoTimer.current = setTimeout(() => {
      undoTimer.current = undefined;
      setUndoState(null);
    }, UNDO_MS);
  }

  function undoSave() {
    clearUndo();
    const before = beforeRef.current;
    if (before === undefined) {
      return;
    }
    beforeRef.current = undefined;
    undoableRef.current = false;
    setPending(before);
    // A save in flight finishes first; run() then saves the restored layout.
    drain();
  }

  function onMoveEnd(layout: Layout) {
    const base = pendingRef.current ?? latest.current.definition;
    const widgets = withLayouts(base.widgets, layout);
    if (widgets === base.widgets) {
      return;
    }
    if (pendingRef.current === undefined) {
      beforeRef.current = latest.current.definition;
    }
    undoableRef.current = true;
    clearUndo();
    setPending({ widgets });
    armDelay();
  }

  /** Saves the pending move at once, or as soon as the running save ends. */
  function flush() {
    if (delayTimer.current !== undefined) {
      clearTimeout(delayTimer.current);
      delayTimer.current = undefined;
    }
    if (pendingRef.current === undefined) {
      return;
    }
    // The move leaves the grid in Edit's hands: no Undo is offered for its save.
    undoableRef.current = false;
    if (savingRef.current) {
      flushRef.current = true;
      return;
    }
    drain();
  }

  // Edit starting, a hidden dashboard, a narrow window or a role change: an Undo would revert a
  // later Edit save, so it goes, and a pending move is saved now.
  // biome-ignore lint/correctness/useExhaustiveDependencies: flush and clearUndo read refs and stable setters only
  useEffect(() => {
    if (!enabled) {
      clearUndo();
      flush();
    }
  }, [enabled]);

  // Leaving the page saves a pending move and drops the timers.
  // biome-ignore lint/correctness/useExhaustiveDependencies: flush reads refs and stable setters only
  useEffect(
    () => () => {
      flush();
      if (delayTimer.current !== undefined) {
        clearTimeout(delayTimer.current);
        delayTimer.current = undefined;
      }
      if (undoTimer.current !== undefined) {
        clearTimeout(undoTimer.current);
        undoTimer.current = undefined;
      }
    },
    [],
  );

  return { pending, onMoveEnd, undo, saving };
}
