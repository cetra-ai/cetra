import { CopyButton } from "@/components/copy-button";
import { Label } from "@/components/ui/label";

/** A value that cannot be edited, shown in monospace with a copy button. */
export function ReadOnlyCodeField({
  id,
  label,
  value,
  description,
}: {
  id: string;
  label: string;
  value: string;
  description?: string;
}) {
  return (
    <div className="flex flex-col gap-1.5">
      <Label htmlFor={id}>{label}</Label>
      <div className="flex items-center gap-1">
        <code id={id} className="rounded-md border bg-muted px-2 py-1 font-mono text-body">
          {value}
        </code>
        <CopyButton value={value} label={label.toLowerCase()} />
      </div>
      {description ? <p className="text-caption text-muted-foreground">{description}</p> : null}
    </div>
  );
}
