import { X } from "lucide-react";
import type { Schema } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

type Mapping = Schema<"TemplateMapping">;
type Target = "trace" | "span";

export interface MappingRow {
  id: number;
  variable: string;
  expression: string;
}

let rowId = 0;

/** A row id that no other row shares, whatever kind of row it belongs to. */
export function nextRowId(): number {
  rowId += 1;
  return rowId;
}

export function newMappingRow(variable: string, expression: string): MappingRow {
  return { id: nextRowId(), variable, expression };
}

/** `{{ name }}` variables of a prompt, in order of first use. */
export function promptVariables(prompt: string): string[] {
  const names = new Set<string>();
  for (const match of prompt.matchAll(/\{\{\s*([A-Za-z_][A-Za-z0-9_]*)/g)) {
    const name = match[1];
    if (name !== undefined) {
      names.add(name);
    }
  }
  return [...names];
}

/** The usual expression of a variable on a target; other variables start empty. */
export function defaultExpression(variable: string, target: Target): string {
  if (variable === "input") {
    return `${target}.input`;
  }
  if (variable === "output") {
    return `${target}.output`;
  }
  return "";
}

export function initialMapping(
  defaults: Mapping[],
  prompt: string,
  target: Target = "trace",
): MappingRow[] {
  if (defaults.length > 0) {
    return defaults.map((row) => newMappingRow(row.variable, row.expression));
  }
  return promptVariables(prompt).map((variable) =>
    newMappingRow(variable, defaultExpression(variable, target)),
  );
}

/** The mapping an evaluator starts from on a target: its variables with their usual expressions. */
export function defaultMappingFor(evaluator: Schema<"Evaluator">, target: Target): MappingRow[] {
  const config: Record<string, unknown> = evaluator.config;
  let variables: string[];
  if (evaluator.kind === "llm_judge") {
    variables = promptVariables(typeof config.prompt === "string" ? config.prompt : "");
  } else if (evaluator.kind === "check") {
    const values = Array.isArray(config.values) ? config.values : [];
    const needsExpected =
      config.check === "exact_match" ||
      config.check === "levenshtein" ||
      (config.check === "contains" && values.length === 0);
    variables = needsExpected ? ["output", "expected"] : ["output"];
  } else {
    variables = ["input", "output"];
  }
  return variables.map((variable) => newMappingRow(variable, defaultExpression(variable, target)));
}

/** The request's `variable_mapping`: rows with an empty variable are left out. */
export function mappingObject(rows: MappingRow[]): Record<string, string> {
  const result: Record<string, string> = {};
  for (const row of rows) {
    const variable = row.variable.trim();
    if (variable.length > 0) {
      result[variable] = row.expression.trim();
    }
  }
  return result;
}

export function MappingTable({
  rows,
  onChange,
  disabled = false,
  errorFor,
}: {
  rows: MappingRow[];
  onChange: (rows: MappingRow[]) => void;
  disabled?: boolean;
  /** The message for a row's variable, shown under that row. */
  errorFor?: (variable: string) => string | undefined;
}) {
  function edit(id: number, change: Partial<Omit<MappingRow, "id">>) {
    onChange(rows.map((item) => (item.id === id ? { ...item, ...change } : item)));
  }

  return (
    <div className="flex flex-col gap-2">
      <span className="text-sm leading-none font-medium">Variable mapping</span>
      {rows.map((row, index) => {
        const error = errorFor?.(row.variable);
        return (
          <div key={row.id} className="flex flex-col gap-1">
            <div className="flex items-center gap-2">
              <Input
                aria-label={`Mapping variable ${index + 1}`}
                placeholder="Variable"
                value={row.variable}
                disabled={disabled}
                aria-invalid={error ? true : undefined}
                onChange={(event) => {
                  edit(row.id, { variable: event.target.value });
                }}
              />
              <Input
                aria-label={`Mapping expression ${index + 1}`}
                placeholder="trace.input"
                value={row.expression}
                disabled={disabled}
                aria-invalid={error ? true : undefined}
                onChange={(event) => {
                  edit(row.id, { expression: event.target.value });
                }}
              />
              <Button
                type="button"
                variant="ghost"
                size="icon-xs"
                aria-label={`Remove mapping ${index + 1}`}
                disabled={disabled}
                onClick={() => {
                  onChange(rows.filter((item) => item.id !== row.id));
                }}
              >
                <X />
              </Button>
            </div>
            {error ? (
              <p role="alert" className="text-xs text-destructive">
                {error}
              </p>
            ) : null}
          </div>
        );
      })}
      <div>
        <Button
          type="button"
          variant="outline"
          size="sm"
          disabled={disabled}
          onClick={() => {
            onChange([...rows, newMappingRow("", "")]);
          }}
        >
          Add variable
        </Button>
      </div>
    </div>
  );
}
