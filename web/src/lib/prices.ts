import { problemOf } from "@/api/problem";
import type { Schema } from "@/api/types";

export type PriceRow = Schema<"ModelPrice">;
export type PriceInput = Schema<"ModelPriceInput">;

/** The server accepts up to 8 long-context bands per row (D-T-4). */
export const MAX_TIERS = 8;

/** `$2.50 / 1M` style text for one unit price, or "—" when absent. */
export function formatUnitPrice(value: number | undefined): string {
  if (value === undefined || Number.isNaN(value)) {
    return "—";
  }
  if (value === 0) {
    return "$0";
  }
  if (Number.isInteger(value)) {
    return `$${String(value)}`;
  }
  const decimals = String(value).split(".")[1]?.length ?? 0;
  return `$${value.toFixed(Math.max(2, Math.min(6, decimals)))}`;
}

/** The form's state: every field a string, as typed. */
export interface PriceForm {
  modelName: string;
  /** "exact": the pattern is the escaped name; "regex": `pattern` is used as typed. */
  matchKind: "exact" | "regex";
  pattern: string;
  provider: string;
  /** `YYYY-MM-DD` or "" for "always". Interpreted as 00:00 UTC. */
  validFrom: string;
  input: string;
  output: string;
  cacheRead: string;
  cacheWrite: string;
  reasoning: string;
  tiers: TierForm[];
}

export interface TierForm {
  id: number;
  minInputTokens: string;
  input: string;
  output: string;
  cacheRead: string;
  cacheWrite: string;
  reasoning: string;
}

let tierFormId = 0;

/** An empty band with a fresh id, so React keys and error ids stay unique within a form. */
export function newTierForm(): TierForm {
  tierFormId += 1;
  return {
    id: tierFormId,
    minInputTokens: "",
    input: "",
    output: "",
    cacheRead: "",
    cacheWrite: "",
    reasoning: "",
  };
}

const EPOCH_PREFIX = "1970-01-01T00:00:00";

function blankForm(): PriceForm {
  return {
    modelName: "",
    matchKind: "exact",
    pattern: "",
    provider: "",
    validFrom: "",
    input: "",
    output: "",
    cacheRead: "",
    cacheWrite: "",
    reasoning: "",
    tiers: [],
  };
}

/** An empty form, or the form of an existing row. A row whose pattern equals `escapeRegex(model_name)` opens as "exact". */
export function priceForm(row?: PriceRow): PriceForm {
  if (row === undefined) {
    return blankForm();
  }
  const exact = row.match_pattern === escapeRegex(row.model_name);
  return {
    modelName: row.model_name,
    matchKind: exact ? "exact" : "regex",
    pattern: exact ? "" : row.match_pattern,
    provider: row.provider ?? "",
    validFrom: isAlwaysValid(row.valid_from) ? "" : row.valid_from.slice(0, 10),
    input: numberText(row.price_input),
    output: numberText(row.price_output),
    cacheRead: numberText(row.price_cache_read),
    cacheWrite: numberText(row.price_cache_write),
    reasoning: numberText(row.price_reasoning),
    tiers: row.tiers.map((tier) => ({
      id: newTierForm().id,
      minInputTokens: String(tier.min_input_tokens),
      input: numberText(tier.price_input),
      output: numberText(tier.price_output),
      cacheRead: numberText(tier.price_cache_read),
      cacheWrite: numberText(tier.price_cache_write),
      reasoning: numberText(tier.price_reasoning),
    })),
  };
}

function numberText(value: number | undefined): string {
  return value === undefined ? "" : String(value);
}

/** Sets `key` on `target` only when the text is not blank. */
function priceField<K extends string>(
  target: Partial<Record<K, number>>,
  key: K,
  text: string,
): void {
  if (text.trim() !== "") {
    target[key] = Number(text);
  }
}

function tierBody(tier: TierForm): Schema<"ModelPriceTier"> {
  const body: Schema<"ModelPriceTier"> = { min_input_tokens: Number(tier.minInputTokens) };
  priceField(body, "price_input", tier.input);
  priceField(body, "price_output", tier.output);
  priceField(body, "price_cache_read", tier.cacheRead);
  priceField(body, "price_cache_write", tier.cacheWrite);
  priceField(body, "price_reasoning", tier.reasoning);
  return body;
}

/** The request body. Blank prices are left out; `valid_from` is `<date>T00:00:00.000000000Z` or left out; the pattern is left out for "exact". */
export function priceBody(form: PriceForm): PriceInput {
  const body: PriceInput = { model_name: form.modelName.trim(), tiers: form.tiers.map(tierBody) };
  if (form.matchKind === "regex") {
    body.match_pattern = form.pattern.trim();
  }
  const provider = form.provider.trim().toLowerCase();
  if (provider !== "") {
    body.provider = provider;
  }
  if (form.validFrom !== "") {
    body.valid_from = `${form.validFrom}T00:00:00.000000000Z`;
  }
  priceField(body, "price_input", form.input);
  priceField(body, "price_output", form.output);
  priceField(body, "price_cache_read", form.cacheRead);
  priceField(body, "price_cache_write", form.cacheWrite);
  priceField(body, "price_reasoning", form.reasoning);
  return body;
}

function invalidNumber(text: string): boolean {
  if (text.trim() === "") {
    return false;
  }
  const value = Number(text);
  return !Number.isFinite(value) || value < 0;
}

const PRICE_FIELDS = [
  ["input", "price_input"],
  ["output", "price_output"],
  ["cacheRead", "price_cache_read"],
  ["cacheWrite", "price_cache_write"],
  ["reasoning", "price_reasoning"],
] as const;

/** Client-side checks that need no server: a name, at least an input or an output price, numbers ≥ 0. Keys are the server's field names (`model_name`, `price_input`, `price_output`, …, `tiers[i].min_input_tokens`). */
export function checkPriceForm(form: PriceForm): Record<string, string> {
  const errors: Record<string, string> = {};
  if (form.modelName.trim() === "") {
    errors.model_name = "Enter a name.";
  }
  if (form.matchKind === "regex" && form.pattern.trim() === "") {
    errors.match_pattern = "Enter a pattern, or choose Exact name.";
  }
  for (const [formKey, serverKey] of PRICE_FIELDS) {
    if (invalidNumber(form[formKey])) {
      errors[serverKey] = "Enter a number of 0 or more.";
    }
  }
  if (form.input.trim() === "" && form.output.trim() === "") {
    errors.price_input = "Enter an input or an output price.";
  }
  form.tiers.forEach((tier, index) => {
    const minimum = Number(tier.minInputTokens);
    if (!Number.isInteger(minimum) || minimum < 1 || tier.minInputTokens.trim() === "") {
      errors[`tiers[${index}].min_input_tokens`] = "Enter a whole number of 1 or more.";
    }
    for (const [formKey, serverKey] of PRICE_FIELDS) {
      if (invalidNumber(tier[formKey])) {
        errors[`tiers[${index}].${serverKey}`] = "Enter a number of 0 or more.";
      }
    }
  });
  return errors;
}

/** Rust's `regex::escape`: every one of `\.+*?()|[]{}^$#&-~` gets a backslash. */
export function escapeRegex(text: string): string {
  return text.replace(/[\\.+*?()|[\]{}^$#&\-~]/g, "\\$&");
}

/** The form's message for a `409` that a budgeted online rule causes (D-T-8), or undefined for any other error. */
export function blockedByRules(error: unknown): string | undefined {
  const problem = problemOf(error);
  if (problem?.status !== 409) {
    return undefined;
  }
  const names = new Set<string>();
  for (const item of problem.errors ?? []) {
    if (item.field.startsWith("rules.")) {
      // D-T-8 messages are `<rule name>: …`.
      names.add(item.message.split(":")[0] ?? item.message);
    }
  }
  if (names.size === 0) {
    return undefined;
  }
  return `Budgeted online rules use this price: ${[...names].join(", ")}.`;
}

/** True when `valid_from` is the epoch: the row applies at any time ("Always"). */
export function isAlwaysValid(validFrom: string): boolean {
  return validFrom.startsWith(EPOCH_PREFIX);
}
