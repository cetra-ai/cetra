import { ChevronRight } from "lucide-react";
import { createContext, type ReactNode, useContext, useState } from "react";
import { createPortal } from "react-dom";

type SlotContextValue = {
  slot: HTMLElement | null;
  setSlot: (slot: HTMLElement | null) => void;
};

const SlotContext = createContext<SlotContextValue | null>(null);

/** Holds the top bar's item slot: the element pages portal their crumb into. */
export function TopBarSlotProvider({ children }: { children: ReactNode }) {
  const [slot, setSlot] = useState<HTMLElement | null>(null);
  return <SlotContext.Provider value={{ slot, setSlot }}>{children}</SlotContext.Provider>;
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
