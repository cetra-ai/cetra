const NUMERIC_TYPES = new Set(["integer", "float", "decimal"]);

/** `integer`, `float` and `decimal` columns. */
export function isNumericType(type: string): boolean {
  return NUMERIC_TYPES.has(type);
}

/** `timestamp` columns. */
export function isTimestampType(type: string): boolean {
  return type === "timestamp";
}

/** `date` columns. */
export function isDateType(type: string): boolean {
  return type === "date";
}

/** `string` columns. */
export function isTextType(type: string): boolean {
  return type === "string";
}

/** `boolean` columns. */
export function isBooleanType(type: string): boolean {
  return type === "boolean";
}

const NESTED_TYPES = new Set(["string_list", "string_map", "list", "json"]);

/** `string_list`, `string_map`, `list` and `json` columns. */
export function isNestedType(type: string): boolean {
  return NESTED_TYPES.has(type);
}
