import { fieldErrors } from "@/api/problem";
import { Textarea } from "@/components/ui/textarea";

/** The item fields a dataset form places an error under (the rest go in the form error). */
export const ITEM_FIELDS = ["input", "expected_output", "metadata"] as const;

export type ItemField = (typeof ITEM_FIELDS)[number];

/**
 * Splits a problem's field errors into those placed under an item field and the messages of the
 * others. `items[0].input` and `input.key` both count as `input`.
 */
export function splitItemErrors(caught: unknown): {
  placed: Partial<Record<ItemField, string>>;
  other: string[];
} {
  const placed: Partial<Record<ItemField, string>> = {};
  const other: string[] = [];
  for (const [field, message] of Object.entries(fieldErrors(caught))) {
    const name = field.replace(/^items\[\d+\]\./, "");
    const known = ITEM_FIELDS.find(
      (candidate) =>
        name === candidate || name.startsWith(`${candidate}.`) || name.startsWith(`${candidate}[`),
    );
    if (known) {
      placed[known] = message;
    } else {
      other.push(message);
    }
  }
  return { placed, other };
}

/** A labelled JSON textarea of an item form, with its field error under it. */
export function JsonField({
  label,
  value,
  onChange,
  error,
  disabled,
  rows = 4,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  error?: string;
  disabled?: boolean;
  rows?: number;
}) {
  return (
    <div className="flex flex-col gap-1.5">
      <span className="text-body font-medium">{label}</span>
      <Textarea
        aria-label={label}
        className="font-mono text-caption"
        spellCheck={false}
        rows={rows}
        disabled={disabled}
        value={value}
        aria-invalid={error ? true : undefined}
        onChange={(event) => {
          onChange(event.target.value);
        }}
      />
      {error ? <p className="text-caption text-destructive">{error}</p> : null}
    </div>
  );
}
