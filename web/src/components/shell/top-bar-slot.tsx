import { ChevronRight } from "lucide-react";
import { createContext, type ReactNode, useContext, useState } from "react";
import { createPortal } from "react-dom";

type SlotContextValue = {
  slot: HTMLElement | null;
  setSlot: (slot: HTMLElement | null) => void;
  start: HTMLElement | null;
  setStart: (start: HTMLElement | null) => void;
};

const SlotContext = createContext<SlotContextValue | null>(null);

/**
 * Holds the top bar's two slots: the item slot pages portal their crumb into, and the start slot at
 * the bar's far left (the project sidebar's toggle).
 */
export function TopBarSlotProvider({ children }: { children: ReactNode }) {
  const [slot, setSlot] = useState<HTMLElement | null>(null);
  const [start, setStart] = useState<HTMLElement | null>(null);
  return (
    <SlotContext.Provider value={{ slot, setSlot, start, setStart }}>
      {children}
    </SlotContext.Provider>
  );
}

/** The start slot, rendered by the top bar before the product mark. */
export function TopBarStartTarget() {
  const context = useContext(SlotContext);
  return <span ref={context?.setStart} className="flex items-center empty:hidden" />;
}

/**
 * Content for the top bar's far left: the project sidebar puts its toggle there, so the toggle stays
 * in the same place whether the sidebar is open or collapsed. Nothing without the layout.
 */
export function TopBarStart({ children }: { children: ReactNode }) {
  const context = useContext(SlotContext);
  if (!context?.start) {
    return null;
  }
  return createPortal(children, context.start);
}

/** The slot, rendered by the top bar after the page crumb. */
export function TopBarSlotTarget() {
  const context = useContext(SlotContext);
  return <span ref={context?.setSlot} className="flex min-w-0 items-center gap-1.5" />;
}

/**
 * A page's last crumb: rendered into the top bar after "›", or nothing when the bar has no slot
 * (a page rendered alone in a test, or a page with no page crumb). Render at most one at a time.
 */
export function TopBarCrumb({ children }: { children: ReactNode }) {
  const context = useContext(SlotContext);
  if (!context?.slot) {
    return null;
  }
  return createPortal(
    <>
      <ChevronRight aria-hidden className="size-3.5 shrink-0 text-muted-foreground" />
      {children}
    </>,
    context.slot,
  );
}
