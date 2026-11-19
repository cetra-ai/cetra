import type { Schema } from "@/api/types";

export type ScoreTone = "good" | "mid" | "bad";

type Definition = Schema<"ScoreDefinition">;

/** Position of `value` in [min, max], clamped to [0, 1]. Undefined when the range is empty. */
function position(value: number, min: number, max: number): number | undefined {
  if (!(max > min)) {
    return undefined;
  }
  return Math.min(1, Math.max(0, (value - min) / (max - min)));
}

/** Flips a goodness for `lower`, where a smaller value is better. */
function oriented(g: number, def: Definition): number | undefined {
  switch (def.direction) {
    case "higher":
      return g;
    case "lower":
      return 1 - g;
    case "none":
      return undefined;
  }
}

/**
 * How good one score value is, in [0, 1], under its definition's direction (SD-4).
 * Undefined when the score is neutral (direction `none`), its type differs from the
 * definition's, or the range needed to place it is missing.
 */
export function goodness(value: Schema<"ScoreValue">, def: Definition): number | undefined {
  if (def.direction === "none" || value.data_type !== def.data_type) {
    return undefined;
  }
  switch (value.data_type) {
    case "numeric": {
      if (def.min_value === undefined || def.max_value === undefined) {
        return undefined;
      }
      const g = position(value.value, def.min_value, def.max_value);
      return g === undefined ? undefined : oriented(g, def);
    }
    case "boolean":
      return oriented(value.value ? 1 : 0, def);
    case "categorical": {
      const label = def.categories.find((category) => category.label === value.value.label);
      const values = def.categories.flatMap((category) =>
        category.value === undefined ? [] : [category.value],
      );
      if (label?.value === undefined || values.length === 0) {
        return undefined;
      }
      const g = position(label.value, Math.min(...values), Math.max(...values));
      return g === undefined ? undefined : oriented(g, def);
    }
    case "text":
      return undefined;
  }
}

/**
 * How good a user's average score is, in [0, 1] (SD-4). A boolean average (the share of
 * `true`) is the goodness itself. Only numeric and boolean averages have a tone.
 */
export function averageGoodness(
  avg: number,
  dataType: Schema<"ScoreDataType">,
  def: Definition,
): number | undefined {
  if (def.direction === "none" || dataType !== def.data_type) {
    return undefined;
  }
  if (dataType === "boolean") {
    return oriented(avg, def);
  }
  if (dataType === "numeric") {
    if (def.min_value === undefined || def.max_value === undefined) {
      return undefined;
    }
    const g = position(avg, def.min_value, def.max_value);
    return g === undefined ? undefined : oriented(g, def);
  }
  return undefined;
}

/** Good from two thirds up, bad down to a third, mid in between (SD-5). */
export function toneOf(g: number): ScoreTone {
  if (g >= 2 / 3) {
    return "good";
  }
  if (g <= 1 / 3) {
    return "bad";
  }
  return "mid";
}
