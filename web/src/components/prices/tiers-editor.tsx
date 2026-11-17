import { X } from "lucide-react";
import { TextField } from "@/components/form/text-field";
import { Button } from "@/components/ui/button";
import { MAX_TIERS, newTierForm, type TierForm } from "@/lib/prices";

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

export function TiersEditor({
  tiers,
  onChange,
  errors,
  disabled,
}: {
  tiers: TierForm[];
  onChange: (tiers: TierForm[]) => void;
  errors: Record<string, string>;
  disabled?: boolean;
}) {
  function update(id: number, change: Partial<TierForm>) {
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
            <div className="grid grid-cols-2 gap-3 sm:grid-cols-5">
              {PRICE_PARTS.map((part) => (
                <TextField
                  key={part.key}
                  id={`tier-${tier.id}-${part.key}`}
                  type="number"
                  label={part.label}
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
