import { useRouter } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import type { ReactNode } from "react";
import { useBackTo } from "@/lib/list-return";

/** The class of a header's back link, shared by the link it returns to and its fallback. */
export const BACK_LINK_CLASS =
  "focus-ring -ml-1 inline-flex items-center gap-0.5 rounded-sm px-1 text-body text-muted-foreground hover:bg-row-hover hover:text-foreground";

/**
 * The header's back link: the page the detail page was opened from, when it has one. Without one,
 * `fallback` (the list the page belongs to) is shown.
 */
export function BackLink({ fallback }: { fallback: ReactNode }) {
  const backTo = useBackTo();
  const router = useRouter();
  if (!backTo) {
    return fallback;
  }
  return (
    <a
      href={backTo.href}
      aria-label={`Back to ${backTo.label}`}
      onClick={(event) => {
        // Modified and non-primary clicks keep the browser's own behaviour, such as a new tab.
        if (
          event.button !== 0 ||
          event.metaKey ||
          event.ctrlKey ||
          event.shiftKey ||
          event.altKey
        ) {
          return;
        }
        event.preventDefault();
        router.history.push(backTo.href);
      }}
      className={BACK_LINK_CLASS}
    >
      <ChevronLeft aria-hidden="true" className="size-3.5" />
      {backTo.label}
    </a>
  );
}
