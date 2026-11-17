import {
  cloneElement,
  type FocusEvent,
  type PointerEvent,
  type ReactElement,
  type ReactNode,
  useState,
} from "react";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";

type TriggerHandlers = {
  onPointerEnter?: (event: PointerEvent<Element>) => void;
  onFocus?: (event: FocusEvent<Element>) => void;
};

/**
 * A tooltip that costs nothing until it is needed. Before the user points at or focuses the
 * child, only the child renders. On the first pointer enter or focus the real tooltip mounts,
 * opened at once, and stays mounted.
 */
export function LazyTooltip({
  content,
  children,
  asChild = true,
}: {
  content: ReactNode;
  children: ReactElement<TriggerHandlers>;
  asChild?: boolean;
}) {
  const [mounted, setMounted] = useState(false);
  if (!mounted) {
    return cloneElement(children, {
      onPointerEnter: (event: PointerEvent<Element>) => {
        children.props.onPointerEnter?.(event);
        setMounted(true);
      },
      onFocus: (event: FocusEvent<Element>) => {
        children.props.onFocus?.(event);
        setMounted(true);
      },
    });
  }
  return (
    <Tooltip defaultOpen>
      <TooltipTrigger asChild={asChild}>{children}</TooltipTrigger>
      <TooltipContent>{content}</TooltipContent>
    </Tooltip>
  );
}
