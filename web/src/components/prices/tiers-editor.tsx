import { X } from "lucide-react";
import { TextField } from "@/components/form/text-field";
import { Button } from "@/components/ui/button";
import { MAX_TIERS, newTierForm, type TierForm } from "@/lib/prices";

/** The unit of every price field: prices are USD per one million tokens. */
export const PRICE_UNIT = "USD / 1M tokens";

/** Each band field and the server's name for its error (`tiers[i].<name>`). */
const PRICE_PARTS: {
  key: keyof Omit<TierForm, "id" | "minInputTokens">;
  label: string;
  name: string;
}[] = [
  { key: "input", label: "Input", name: "price_input" },
  { key: "output", label: "Output", name: "price_output" },
  { key: "cacheRead", label: "Cache read", name: "price_cache_read" },
  { key: "cacheWrite", label: "Cache write", name: "price_cache_write" },
  { key: "reasoning", label: "Reasoning", name: "price_reasoning" },
];

/** The server's error name for a changed field of tier `index`. */
function errorName(index: number, key: string): string {
  const part = PRICE_PARTS.find((item) => item.key === key);
  return `tiers[${index}].${part?.name ?? "min_input_tokens"}`;
}

export function TiersEditor({
  tiers,
  onChange,
  onTouch,
  errors,
  disabled,
}: {
  tiers: TierForm[];
  onChange: (tiers: TierForm[]) => void;
  /** Called with the error names of the fields a user changed. */
  onTouch?: (names: string[]) => void;
  errors: Record<string, string>;
  disabled?: boolean;
}) {
  function update(id: number, change: Partial<TierForm>) {
    const index = tiers.findIndex((tier) => tier.id === id);
    onTouch?.(Object.keys(change).map((key) => errorName(index, key)));
    onChange(tiers.map((tier) => (tier.id === id ? { ...tier, ...change } : tier)));
  }

  return (
    <div className="flex flex-col gap-4">
      {tiers.map((tier, index) => {
        const rowError = errors[`tiers[${index}]`];
        return (
          <fieldset key={tier.id} className="flex flex-col gap-3 rounded-md border p-3">
            <legend className="sr-only">{`Tier ${index + 1}`}</legend>
            <div className="flex items-end gap-2">
              <div className="flex-1">
                <TextField
                  id={`tier-${tier.id}-min`}
                  type="number"
                  label="From input tokens"
                  value={tier.minInputTokens}
                  error={errors[`tiers[${index}].min_input_tokens`]}
                  disabled={disabled}
                  min={1}
                  step={1}
                  onChange={(value) => {
                    update(tier.id, { minInputTokens: value });
                  }}
                />
              </div>
              <Button
                type="button"
                variant="ghost"
                size="icon-xs"
                aria-label="Remove tier"
                disabled={disabled}
                onClick={() => {
                  onChange(tiers.filter((item) => item.id !== tier.id));
                }}
              >
                <X />
              </Button>
            </div>
            <div className="grid grid-cols-2 gap-3">
              {PRICE_PARTS.map((part) => (
                <TextField
                  key={part.key}
                  id={`tier-${tier.id}-${part.key}`}
                  type="number"
                  label={part.label}
                  unit={PRICE_UNIT}
                  min={0}
                  value={tier[part.key]}
                  error={errors[`tiers[${index}].${part.name}`]}
                  disabled={disabled}
                  onChange={(value) => {
                    update(tier.id, { [part.key]: value });
                  }}
                />
              ))}
            </div>
            {rowError ? (
              <p role="alert" className="text-caption text-destructive">
                {rowError}
              </p>
            ) : null}
          </fieldset>
        );
      })}
      <div className="flex items-center gap-3">
        <Button
          type="button"
          variant="outline"
          size="sm"
          disabled={disabled || tiers.length >= MAX_TIERS}
          onClick={() => {
            onChange([...tiers, newTierForm()]);
          }}
        >
          Add tier
        </Button>
        {tiers.length >= MAX_TIERS ? (
          <p className="text-caption text-muted-foreground">At most {MAX_TIERS} tiers.</p>
        ) : null}
      </div>
    </div>
  );
}
