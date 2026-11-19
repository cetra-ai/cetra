import { cn } from "cn";
import type { LucideIcon } from "lucide-react";
import type { ComponentProps, ReactNode } from "react";

/** An icon and a value. `label` is read by screen readers and not shown. Spreads its props, so it can be a `TooltipTrigger asChild` child. */
export function MetricChip({
  icon: Icon,
  label,
  children,
  className,
  ...props
}: ComponentProps<"span"> & {
  icon: LucideIcon;
  label: string;
  children: ReactNode;
}) {
  return (
    <span
      {...props}
      className={cn(
        "inline-flex items-center gap-1 rounded-md border px-1.5 py-0.5 text-body",
        className,
      )}
    >
      <Icon aria-hidden="true" className="size-3.5 text-muted-foreground" />
      <span className="sr-only">{label}</span>
      <span className="num text-foreground">{children}</span>
    </span>
  );
}
