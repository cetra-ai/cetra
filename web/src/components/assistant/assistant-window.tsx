import {
  type ReactNode,
  type PointerEvent as ReactPointerEvent,
  useEffect,
  useRef,
  useState,
} from "react";
import {
  fitWindow,
  readWindowRect,
  WINDOW_MIN,
  type WindowRect,
  writeWindowRect,
} from "@/lib/assistant/panel";

/** The attribute that marks the part of the panel that drags the window (its header). */
const WINDOW_HANDLE = "data-window-handle";

/** Controls inside the handle that keep their own pointer behaviour. */
const INTERACTIVE = "button, a, input, textarea, select, [role='combobox'], [role='menuitem']";

function viewport() {
  return { width: window.innerWidth, height: window.innerHeight };
}

/**
 * The assistant as a window over the page: dragged by the panel's header, resized from its bottom
 * right corner, kept inside the viewport. Its position and size persist (spec 11 § Assistant panel).
 */
export function AssistantWindow({ children }: { children: ReactNode }) {
  const [rect, setRect] = useState<WindowRect>(() => readWindowRect(viewport()));
  const box = useRef<HTMLDivElement>(null);
  const stopDrag = useRef<(() => void) | undefined>(undefined);

  useEffect(() => {
    writeWindowRect(rect);
  }, [rect]);

  // A smaller browser window pulls the window back inside it.
  useEffect(() => {
    const onResize = () => {
      setRect((current) => fitWindow(current, viewport()));
    };
    window.addEventListener("resize", onResize);
    return () => {
      window.removeEventListener("resize", onResize);
    };
  }, []);

  // The corner grip resizes the element itself; the observer copies its size into the state.
  useEffect(() => {
    const element = box.current;
    if (!element) {
      return;
    }
    const observer = new ResizeObserver(() => {
      const width = element.offsetWidth;
      const height = element.offsetHeight;
      setRect((current) =>
        current.width === width && current.height === height
          ? current
          : fitWindow({ ...current, width, height }, viewport()),
      );
    });
    observer.observe(element);
    return () => {
      observer.disconnect();
    };
  }, []);

  useEffect(
    () => () => {
      stopDrag.current?.();
    },
    [],
  );

  const startDrag = (event: ReactPointerEvent<HTMLDivElement>) => {
    const target = event.target as HTMLElement;
    if (
      event.button !== 0 ||
      target.closest(`[${WINDOW_HANDLE}]`) === null ||
      target.closest(INTERACTIVE) !== null
    ) {
      return;
    }
    event.preventDefault();
    const start = { pointerX: event.clientX, pointerY: event.clientY, x: rect.x, y: rect.y };
    const move = (moved: PointerEvent) => {
      setRect((current) =>
        fitWindow(
          {
            ...current,
            x: start.x + moved.clientX - start.pointerX,
            y: start.y + moved.clientY - start.pointerY,
          },
          viewport(),
        ),
      );
    };
    const stop = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
      stopDrag.current = undefined;
    };
    stopDrag.current?.();
    stopDrag.current = stop;
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
    window.addEventListener("pointercancel", stop);
  };

  return (
    <div
      ref={box}
      data-testid="assistant-window"
      className="fixed z-40 flex resize flex-col overflow-hidden rounded-lg border bg-background shadow-2xl"
      style={{
        left: rect.x,
        top: rect.y,
        width: rect.width,
        height: rect.height,
        minWidth: WINDOW_MIN.width,
        minHeight: WINDOW_MIN.height,
      }}
      onPointerDown={startDrag}
    >
      {children}
    </div>
  );
}
