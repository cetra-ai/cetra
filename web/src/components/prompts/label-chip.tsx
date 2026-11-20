import { cn } from "cn";
import { Lock, X } from "lucide-react";
import { DisabledReason } from "@/components/disabled-reason";
import { categoryClasses } from "@/lib/colors";
import { PROTECTED_REASON, VIEWER_REASON } from "@/lib/prompts";

/**
 * A label and the version it points at. The colour follows the label's name. A protected label
 * shows a lock (D-V-10). With `onRemove`, a remove button; it is disabled, with the reason, when
 * `canTouch` is false (a viewer, or a protected label below admin).
 */
export function LabelChip({
  label,
  version,
  protectedLabel = false,
  canTouch = true,
  onRemove,
  className,
}: {
  label: string;
  version?: number;
  protectedLabel?: boolean;
  canTouch?: boolean;
  onRemove?: () => void;
  className?: string;
}) {
  const removeReason = canTouch ? undefined : protectedLabel ? PROTECTED_REASON : VIEWER_REASON;
  return (
    <span className={cn(categoryClasses(label).pill, "inline-flex items-center gap-1", className)}>
      <span>{label}</span>
      {version !== undefined ? (
        <span className="num text-muted-foreground">→ v{version}</span>
      ) : null}
      {protectedLabel ? (
        <Lock role="img" aria-label="Protected" className="size-3 text-muted-foreground" />
      ) : null}
      {onRemove ? (
        <DisabledReason reason={removeReason}>
          <button
            type="button"
            aria-label={`Remove label ${label}`}
            disabled={removeReason !== undefined}
            onClick={onRemove}
            className="focus-ring inline-flex items-center rounded-sm hover:text-foreground disabled:pointer-events-none disabled:opacity-50"
          >
            <X aria-hidden="true" className="size-3" />
          </button>
        </DisabledReason>
      ) : null}
    </span>
  );
}
