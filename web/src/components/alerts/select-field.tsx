import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

export type Option<T extends string> = { value: T; label: string };

/** A select that maps the Radix string back to the option's typed value. */
export function SelectField<T extends string>({
  id,
  label,
  value,
  options,
  description,
  disabled,
  onChange,
}: {
  id: string;
  label: string;
  value: T;
  options: Option<T>[];
  description?: string;
  disabled: boolean;
  onChange: (value: T) => void;
}) {
  const descriptionId = `${id}-description`;
  return (
    <div className="flex flex-col gap-1.5">
      <Label htmlFor={id}>{label}</Label>
      <Select
        value={value}
        onValueChange={(next) => {
          const found = options.find((option) => option.value === next);
          if (found) {
            onChange(found.value);
          }
        }}
        disabled={disabled}
      >
        <SelectTrigger
          id={id}
          className="w-full"
          aria-describedby={description ? descriptionId : undefined}
        >
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {options.map((option) => (
            <SelectItem key={option.value} value={option.value}>
              {option.label}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      {description ? (
        <p id={descriptionId} className="text-caption text-muted-foreground">
          {description}
        </p>
      ) : null}
    </div>
  );
}
