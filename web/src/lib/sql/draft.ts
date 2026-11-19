export type SqlDraft = {
  query: string;
  /** Parameter values as typed, by name (D-H-7). */
  params: Record<string, string>;
  /** The saved query this draft was loaded from (D-H-13). */
  view?: string;
};

/** Changes a draft. Always pass a function so two updates in one event both apply. */
export type DraftUpdate = (update: (previous: SqlDraft) => SqlDraft) => void;

export const DEFAULT_QUERY =
  "SELECT name, count(*) AS trace_count\nFROM traces\nGROUP BY name\nORDER BY trace_count DESC\nLIMIT 100";

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
