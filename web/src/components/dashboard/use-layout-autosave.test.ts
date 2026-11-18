import { act, renderHook } from "@testing-library/react";
import type { Layout } from "react-grid-layout";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Definition } from "@/lib/dashboards/types";
import { AUTOSAVE_DELAY_MS, UNDO_MS, useLayoutAutosave } from "./use-layout-autosave";

/** Two widgets, the first one at the top left, the second under it. */
const SAVED: Definition = {
  widgets: [
    { id: "a", title: "A", kind: "heading", layout: { x: 0, y: 0, w: 12, h: 1 } },
    { id: "b", title: "B", kind: "heading", layout: { x: 0, y: 1, w: 6, h: 3 } },
  ],
};

/** The grid's layout after a move of `b` to the given column. */
function moved(x: number): Layout {
  return [
    { i: "a", x: 0, y: 0, w: 12, h: 1 },
    { i: "b", x, y: 1, w: 6, h: 3 },
  ];
}

/** A problem as the API client throws it. */
function problem(status: number, detail: string) {
  return { status, title: "Problem", code: "conflict", detail };
}

type Props = {
  definition: Definition;
  version: number | undefined;
  enabled: boolean;
};

function setup(
  options: {
    persist?: (definition: Definition, version: number | undefined) => Promise<unknown>;
    onConflict?: () => void;
    onError?: (message: string) => void;
    props?: Partial<Props>;
  } = {},
) {
  const persist = vi.fn(options.persist ?? (async () => undefined));
  const onConflict = vi.fn(options.onConflict ?? (() => undefined));
  const onError = vi.fn(options.onError ?? (() => undefined));
  const initial: Props = { definition: SAVED, version: 1, enabled: true, ...options.props };
  const hook = renderHook(
    (props: Props) =>
      useLayoutAutosave({
        definition: props.definition,
        version: props.version,
        enabled: props.enabled,
        persist,
        onConflict,
        onError,
      }),
    { initialProps: initial },
  );
  return { ...hook, persist, onConflict, onError };
}

/** Lets a pending promise and the state it sets settle. */
async function settle() {
  await act(async () => {
    await Promise.resolve();
  });
}

function advance(ms: number) {
  act(() => {
    vi.advanceTimersByTime(ms);
  });
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("the layout autosave", () => {
  it("saves once after the last move", async () => {
    const { result, persist } = setup();
    act(() => result.current.onMoveEnd(moved(2)));
    advance(300);
    act(() => result.current.onMoveEnd(moved(4)));
    advance(300);
    act(() => result.current.onMoveEnd(moved(6)));

    advance(AUTOSAVE_DELAY_MS - 1);
    expect(persist).not.toHaveBeenCalled();
    advance(1);
    await settle();

    expect(persist).toHaveBeenCalledTimes(1);
    const [definition, version] = persist.mock.calls[0] ?? [];
    expect(definition).toEqual({
      widgets: [SAVED.widgets[0], { ...SAVED.widgets[1], layout: { x: 6, y: 1, w: 6, h: 3 } }],
    });
    expect(version).toBe(1);
  });

  it("ignores a move that changes nothing", () => {
    const { result, persist } = setup();
    act(() => result.current.onMoveEnd(moved(0)));
    advance(AUTOSAVE_DELAY_MS * 2);
    expect(persist).not.toHaveBeenCalled();
    expect(result.current.pending).toBeUndefined();
  });

  it("a move during a save waits and is saved next, with the newer version", async () => {
    let release: () => void = () => undefined;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const persist = vi
      .fn<(definition: Definition, version: number | undefined) => Promise<unknown>>()
      .mockImplementationOnce(() => gate)
      .mockImplementation(async () => undefined);
    const { result, rerender } = setup({ persist });

    act(() => result.current.onMoveEnd(moved(2)));
    advance(AUTOSAVE_DELAY_MS);
    expect(result.current.saving).toBe(true);

    act(() => result.current.onMoveEnd(moved(4)));
    advance(AUTOSAVE_DELAY_MS * 3);
    expect(persist).toHaveBeenCalledTimes(1);

    // The first save lands: the cache now holds version 2.
    release();
    await settle();
    rerender({ definition: SAVED, version: 2, enabled: true });

    advance(AUTOSAVE_DELAY_MS);
    await settle();
    expect(persist).toHaveBeenCalledTimes(2);
    expect(persist.mock.calls[1]?.[1]).toBe(2);
    expect(persist.mock.calls[1]?.[0].widgets[1]?.layout.x).toBe(4);
  });

  it("offers undo for 10 s, and undo saves the layout before the moves", async () => {
    const { result, persist } = setup();
    act(() => result.current.onMoveEnd(moved(6)));
    advance(AUTOSAVE_DELAY_MS);
    await settle();
    expect(result.current.undo).not.toBeNull();

    // The offer starts when the save lands.
    advance(UNDO_MS - 1);
    expect(result.current.undo).not.toBeNull();
    advance(1);
    expect(result.current.undo).toBeNull();

    // A fresh move, then undo while its offer is open.
    act(() => result.current.onMoveEnd(moved(2)));
    advance(AUTOSAVE_DELAY_MS);
    await settle();
    expect(result.current.undo).not.toBeNull();
    act(() => result.current.undo?.run());
    await settle();

    expect(persist).toHaveBeenCalledTimes(3);
    expect(persist.mock.calls[2]?.[0]).toEqual(SAVED);
    expect(result.current.undo).toBeNull();
  });

  it("a new move clears the undo", async () => {
    const { result } = setup();
    act(() => result.current.onMoveEnd(moved(6)));
    advance(AUTOSAVE_DELAY_MS);
    await settle();
    expect(result.current.undo).not.toBeNull();

    act(() => result.current.onMoveEnd(moved(2)));
    expect(result.current.undo).toBeNull();
  });

  it("a 409 drops the move and calls onConflict", async () => {
    const { result, onConflict, onError } = setup({
      persist: async () => {
        throw problem(409, "Stale.");
      },
    });
    act(() => result.current.onMoveEnd(moved(6)));
    advance(AUTOSAVE_DELAY_MS);
    await settle();

    expect(onConflict).toHaveBeenCalledTimes(1);
    expect(onError).not.toHaveBeenCalled();
    expect(result.current.pending).toBeUndefined();
    expect(result.current.undo).toBeNull();
  });

  it("another error drops the move and reports the message", async () => {
    const { result, onConflict, onError } = setup({
      persist: async () => {
        throw problem(500, "The database is busy.");
      },
    });
    act(() => result.current.onMoveEnd(moved(6)));
    advance(AUTOSAVE_DELAY_MS);
    await settle();

    expect(onConflict).not.toHaveBeenCalled();
    expect(onError).toHaveBeenCalledWith("The database is busy.");
    expect(result.current.pending).toBeUndefined();
  });

  it("disabling flushes a pending move and clears the undo", async () => {
    const { result, rerender, persist } = setup();
    act(() => result.current.onMoveEnd(moved(6)));
    advance(AUTOSAVE_DELAY_MS);
    await settle();
    expect(result.current.undo).not.toBeNull();

    act(() => result.current.onMoveEnd(moved(2)));
    expect(result.current.pending).toBeDefined();
    rerender({ definition: SAVED, version: 1, enabled: false });
    await settle();

    expect(result.current.undo).toBeNull();
    expect(persist).toHaveBeenCalledTimes(2);
    advance(AUTOSAVE_DELAY_MS * 2);
    expect(persist).toHaveBeenCalledTimes(2);
  });

  it("unmount flushes and clears the timers", async () => {
    const { result, unmount, persist } = setup();
    act(() => result.current.onMoveEnd(moved(6)));
    unmount();
    await settle();

    expect(persist).toHaveBeenCalledTimes(1);
    advance(AUTOSAVE_DELAY_MS * 2 + UNDO_MS);
    expect(persist).toHaveBeenCalledTimes(1);
  });
});
