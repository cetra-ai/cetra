import type { ReactNode } from "react";
import { DisabledReason } from "@/components/disabled-reason";
import { SettingsCard } from "@/components/settings/settings-card";
import { Button } from "@/components/ui/button";

/** A destructive action set apart at the foot of a settings page. */
export function DangerZone({
  title,
  description,
  buttonLabel,
  disabledReason,
  disabled,
  onOpen,
  children,
}: {
  /** For example `Delete project`. */
  title: string;
  description: ReactNode;
  /** For example `Delete project…`. */
  buttonLabel: string;
  /** Set → the button is disabled with this reason (D-L-4). */
  disabledReason?: string;
  /** Disabled without a reason, for example while the role is still loading. */
  disabled?: boolean;
  onOpen: () => void;
  /** The confirmation dialog. */
  children?: ReactNode;
}) {
  return (
    <SettingsCard tone="danger" title="Danger zone">
      <div className="flex items-center justify-between gap-4">
        <div className="flex flex-col gap-1">
          <p className="font-medium">{title}</p>
          <div className="text-body text-muted-foreground">{description}</div>
        </div>
        <DisabledReason reason={disabledReason}>
          <Button
            type="button"
            variant="destructive"
            disabled={disabled === true || disabledReason !== undefined}
            onClick={onOpen}
          >
            {buttonLabel}
          </Button>
        </DisabledReason>
      </div>
      {children}
    </SettingsCard>
  );
}
