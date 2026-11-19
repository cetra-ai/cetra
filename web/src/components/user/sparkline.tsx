import { cn } from "cn";

export function Sparkline({
  values,
  label,
  className,
}: {
  values: readonly number[];
  label: string;
  className?: string;
}) {
  const max = Math.max(1, ...values);
  return (
    <svg
      role="img"
      aria-label={label}
      viewBox={`0 0 ${values.length} 1`}
      preserveAspectRatio="none"
      className={cn("h-4 w-20 text-primary", className)}
    >
      {values.map((value, index) =>
        value > 0 ? (
          <rect
            // biome-ignore lint/suspicious/noArrayIndexKey: bars are positional and never reorder
            key={index}
            x={index + 0.1}
            width={0.8}
            height={value / max}
            y={1 - value / max}
            fill="currentColor"
          />
        ) : null,
      )}
    </svg>
  );
}
