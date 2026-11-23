import { Input } from "@/components/ui/input";
import { InputGroup, InputGroupAddon, InputGroupText } from "@/components/ui/input-group";
import { Label } from "@/components/ui/label";

export function TextField({
  id,
  label,
  value,
  onChange,
  type = "text",
  error,
  description,
  autoComplete,
  required,
  disabled,
  readOnly,
  maxLength,
  placeholder,
  unit,
  inputMode,
  min,
  max,
  step,
  list,
}: {
  id: string;
  label: string;
  value: string;
  onChange: (value: string) => void;
  type?: "text" | "email" | "password" | "number" | "date";
  error?: string;
  description?: string;
  autoComplete?: string;
  required?: boolean;
  disabled?: boolean;
  readOnly?: boolean;
  maxLength?: number;
  placeholder?: string;
  /** Shown after the input, for example `days` or `USD per 1M tokens`. */
  unit?: string;
  inputMode?: "numeric" | "decimal" | "text";
  min?: number;
  max?: number;
  step?: number;
  /** The id of a `<datalist>` of suggestions. */
  list?: string;
}) {
  const describedBy = [description ? `${id}-description` : null, error ? `${id}-error` : null]
    .filter((item): item is string => item !== null)
    .join(" ");
  const input = (
    <Input
      id={id}
      name={id}
      type={type}
      value={value}
      onChange={(event) => {
        onChange(event.target.value);
      }}
      aria-invalid={error ? true : undefined}
      aria-describedby={describedBy.length > 0 ? describedBy : undefined}
      autoComplete={autoComplete}
      required={required}
      disabled={disabled}
      readOnly={readOnly}
      maxLength={maxLength}
      placeholder={placeholder}
      inputMode={inputMode}
      min={min}
      max={max}
      step={step}
      list={list}
    />
  );
  return (
    <div className="flex flex-col gap-1.5">
      <Label htmlFor={id}>{label}</Label>
      {unit === undefined ? (
        input
      ) : (
        <InputGroup>
          {input}
          <InputGroupAddon align="inline-end">
            <InputGroupText>{unit}</InputGroupText>
          </InputGroupAddon>
        </InputGroup>
      )}
      {description ? (
        <p id={`${id}-description`} className="text-caption text-muted-foreground">
          {description}
        </p>
      ) : null}
      {error ? (
        <p id={`${id}-error`} className="text-caption text-destructive">
          {error}
        </p>
      ) : null}
    </div>
  );
}
