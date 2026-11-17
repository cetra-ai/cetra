import { type DiffLine, diffLines } from "@/lib/line-diff";

/**
 * The fields of a version that the diff reads. The generated `config` type is
 * `Record<string, never>`, so this type takes any record.
 */
export interface VersionLike {
  name: string;
  kind: string;
  config: Record<string, unknown>;
  output_score: unknown;
}

export interface FieldChange {
  path: string;
  before: unknown;
  after: unknown;
  kind: "added" | "removed" | "changed";
}

export interface VersionDiff {
  prompt: DiffLine[] | undefined;
  name: FieldChange | undefined;
  config: FieldChange[];
  output: FieldChange[];
  nothingVisible: boolean;
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * The fields that differ between two values, sorted by path. Two plain objects are compared key
 * by key; anything else (arrays, null, scalars, mismatched types) is one leaf. `skip` lists the
 * root's keys to leave out.
 */
export function changedFields(
  before: unknown,
  after: unknown,
  skip: readonly string[] = [],
): FieldChange[] {
  const out: FieldChange[] = [];
  walk(before, after, "", skip, true, out);
  return out.sort((left, right) => (left.path < right.path ? -1 : left.path > right.path ? 1 : 0));
}

function walk(
  before: unknown,
  after: unknown,
  prefix: string,
  skip: readonly string[],
  root: boolean,
  out: FieldChange[],
): void {
  if (isPlainObject(before) && isPlainObject(after)) {
    const keys = [...new Set([...Object.keys(before), ...Object.keys(after)])].sort();
    for (const key of keys) {
      if (root && skip.includes(key)) {
        continue;
      }
      const path = prefix === "" ? key : `${prefix}.${key}`;
      const inBefore = Object.hasOwn(before, key);
      const inAfter = Object.hasOwn(after, key);
      if (!inBefore) {
        out.push({ path, kind: "added", before: undefined, after: after[key] });
      } else if (!inAfter) {
        out.push({ path, kind: "removed", before: before[key], after: undefined });
      } else {
        walk(before[key], after[key], path, skip, false, out);
      }
    }
    return;
  }
  if (JSON.stringify(before) !== JSON.stringify(after)) {
    out.push({ path: prefix, kind: "changed", before, after });
  }
}

function promptOf(version: VersionLike | undefined): string | undefined {
  const prompt = version?.config.prompt;
  return typeof prompt === "string" ? prompt : undefined;
}

/**
 * What changed from `previous` to `current`: the prompt as a line diff (an LLM judge only), the
 * name, the other config fields and the output score. With no `previous` everything is added.
 */
export function versionDiff(previous: VersionLike | undefined, current: VersionLike): VersionDiff {
  const currentPrompt = promptOf(current);
  const prompt =
    current.kind === "llm_judge" && currentPrompt !== undefined
      ? diffLines(promptOf(previous) ?? "", currentPrompt)
      : undefined;
  const name = nameChange(previous, current);
  const config = changedFields(previous?.config ?? {}, current.config, ["prompt"]);
  const output = changedFields(previous?.output_score ?? {}, current.output_score);
  const nothingVisible =
    previous !== undefined &&
    (prompt === undefined || prompt.every((line) => line.op === "same")) &&
    name === undefined &&
    config.length === 0 &&
    output.length === 0;
  return { prompt, name, config, output, nothingVisible };
}

function nameChange(
  previous: VersionLike | undefined,
  current: VersionLike,
): FieldChange | undefined {
  if (previous === undefined) {
    return { path: "name", kind: "added", before: undefined, after: current.name };
  }
  if (previous.name !== current.name) {
    return { path: "name", kind: "changed", before: previous.name, after: current.name };
  }
  return undefined;
}
