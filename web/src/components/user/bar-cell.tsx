import type { ReactNode } from "react";

export function BarCell({
  value,
  max,
  children,
}: {
  value: number | undefined;
  max: number;
  children: ReactNode;
}) {
  const pct = value && max > 0 ? Math.min(100, (value / max) * 100) : 0;
  return (
    <div className="relative flex h-full w-full items-center justify-end">
      <span
        aria-hidden="true"
        data-bar
        className="absolute inset-y-1 right-0 rounded-sm bg-primary/15"
        style={{ width: `${pct}%` }}
      />
      <span className="relative">{children}</span>
    </div>
  );
}
