import { Lock } from "lucide-react";
import { cn } from "cn";
import { categoryClasses } from "@/lib/colors";

/**
 * A label and the version it points at. The colour follows the label's name. A protected label
 * shows a lock (D-V-10).
 */
export function LabelChip({
  label,
  version,
  protectedLabel = false,
  className,
}: {
  label: string;
  version?: number;
  protectedLabel?: boolean;
  className?: string;
}) {
  return (
    <span className={cn(categoryClasses(label).pill, "inline-flex items-center gap-1", className)}>
      <span>{label}</span>
      {version !== undefined ? (
        <span className="num text-muted-foreground">→ v{version}</span>
      ) : null}
      {protectedLabel ? (
        <Lock role="img" aria-label="Protected" className="size-3 text-muted-foreground" />
      ) : null}
    </span>
  );
}
