// SPDX-License-Identifier: Elastic-2.0
import { DisabledReason } from "@/components/disabled-reason";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { overrideLabel } from "@/lib/roles";

export const OVERRIDES = ["admin", "member", "viewer", "none"] as const;

export type Access = "inherit" | (typeof OVERRIDES)[number];

/**
 * The project access picker of one member. `stored` is the saved override and `value` the
 * picked one, so a panel form can hold a draft. When `locked` (the plan has no
 * `project_roles`), new overrides are disabled but the member's stored override can still be
 * removed with "Inherit org role" (D-P-8).
 */
export function ProjectAccessSelect(props: {
  id?: string;
  email: string;
  stored: Access;
  value: Access;
  canEdit: boolean;
  locked: boolean;
  pending: boolean;
  onChange: (value: Access) => void;
}) {
  const { id, email, stored, value, canEdit, locked, pending, onChange } = props;

  return (
    <DisabledReason reason={canEdit ? undefined : "Only project admins can change access"}>
      <Select
        value={value}
        onValueChange={(next) => {
          const choice = (["inherit", ...OVERRIDES] as const).find((item) => item === next);
          if (choice) {
            onChange(choice);
          }
        }}
        disabled={!canEdit || pending}
      >
        <SelectTrigger id={id} aria-label={`Project access of ${email}`} className="h-7 w-40">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {(["inherit", ...OVERRIDES] as const).map((item) => (
            <SelectItem
              key={item}
              value={item}
              disabled={locked && item !== "inherit" && item !== stored}
            >
              {overrideLabel(item)}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </DisabledReason>
  );
}
