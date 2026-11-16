import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import {
  defaultExpression,
  defaultMappingFor,
  MappingTable,
  mappingObject,
  newMappingRow,
} from "@/components/evaluators/mapping-table";

const JUDGE_CONFIG: Record<string, unknown> = {
  connection_id: "01900000-0000-7000-8000-000000000400",
  model: "gpt-5-mini",
  prompt: "Question: {{ input }}\nAnswer: {{ output }} {{ input }}",
  output: { type: "numeric", min: 0, max: 1 },
};

const JUDGE: Schema<"Evaluator"> = {
  id: "01900000-0000-7000-8000-000000000500",
  name: "Relevance",
  kind: "llm_judge",
  config: JUDGE_CONFIG as Schema<"Evaluator">["config"],
  output_score: { name: "relevance", data_type: "numeric" },
  has_signing_secret: false,
  version: 1,
  created_at: "2026-01-01T09:00:00.000000000Z",
  updated_at: "2026-01-01T09:00:00.000000000Z",
};

/** An evaluator of `kind` with `config`; the generated config type is empty, so the test casts it. */
function evaluatorOf(
  kind: Schema<"Evaluator">["kind"],
  config: Record<string, unknown>,
): Schema<"Evaluator"> {
  return {
    ...JUDGE,
    kind,
    config: config as Schema<"Evaluator">["config"],
  };
}

/** The `[variable, expression]` pairs of a list of rows, in order. */
function pairs(rows: { variable: string; expression: string }[]): [string, string][] {
  return rows.map((row) => [row.variable, row.expression]);
}

describe("defaultExpression", () => {
  it("maps input and output to the target", () => {
    expect(defaultExpression("input", "trace")).toBe("trace.input");
    expect(defaultExpression("output", "trace")).toBe("trace.output");
    expect(defaultExpression("input", "span")).toBe("span.input");
    expect(defaultExpression("output", "span")).toBe("span.output");
  });

  it("leaves other variables empty", () => {
    expect(defaultExpression("expected", "trace")).toBe("");
    expect(defaultExpression("expected", "span")).toBe("");
  });
});

describe("defaultMappingFor", () => {
  it("maps each prompt variable of a judge once, in order", () => {
    expect(pairs(defaultMappingFor(JUDGE, "trace"))).toEqual([
      ["input", "trace.input"],
      ["output", "trace.output"],
    ]);
    expect(pairs(defaultMappingFor(JUDGE, "span"))).toEqual([
      ["input", "span.input"],
      ["output", "span.output"],
    ]);
  });

  it("adds expected for the checks that compare with it", () => {
    const check = (config: Record<string, unknown>): Schema<"Evaluator"> => ({
      ...evaluatorOf("check", config),
      output_score: { name: "match", data_type: "boolean" },
    });

    expect(pairs(defaultMappingFor(check({ check: "exact_match" }), "trace"))).toEqual([
      ["output", "trace.output"],
      ["expected", ""],
    ]);
    expect(pairs(defaultMappingFor(check({ check: "contains", values: [] }), "trace"))).toEqual([
      ["output", "trace.output"],
      ["expected", ""],
    ]);
    expect(
      pairs(defaultMappingFor(check({ check: "contains", values: ["refund"] }), "trace")),
    ).toEqual([["output", "trace.output"]]);
  });

  it("maps a webhook's input and output", () => {
    const webhook = evaluatorOf("webhook", {});
    expect(pairs(defaultMappingFor(webhook, "trace"))).toEqual([
      ["input", "trace.input"],
      ["output", "trace.output"],
    ]);
  });
});

describe("mappingObject", () => {
  it("drops rows with an empty variable and trims the rest", () => {
    const rows = [
      newMappingRow("", "trace.input"),
      newMappingRow(" input ", " trace.input "),
      newMappingRow("output", "trace.output"),
    ];
    expect(mappingObject(rows)).toEqual({ input: "trace.input", output: "trace.output" });
  });
});

describe("MappingTable", () => {
  const rows = [newMappingRow("input", "trace.input"), newMappingRow("output", "trace.output")];

  it("adds an empty row and removes a row through onChange", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(<MappingTable rows={rows} onChange={onChange} />);

    await user.click(screen.getByRole("button", { name: "Add variable" }));
    const added = onChange.mock.calls[0]?.[0] as typeof rows;
    expect(pairs(added)).toEqual([
      ["input", "trace.input"],
      ["output", "trace.output"],
      ["", ""],
    ]);

    await user.click(screen.getByRole("button", { name: "Remove mapping 1" }));
    expect(pairs(onChange.mock.calls[1]?.[0] as typeof rows)).toEqual([["output", "trace.output"]]);
  });

  it("shows the error of a row under that row", () => {
    render(
      <MappingTable
        rows={rows}
        onChange={vi.fn()}
        errorFor={(variable) => (variable === "output" ? "expression is too long" : undefined)}
      />,
    );

    expect(screen.getByText("expression is too long")).toBeInTheDocument();
    expect(screen.getByLabelText("Mapping variable 2")).toHaveAttribute("aria-invalid", "true");
    expect(screen.getByLabelText("Mapping variable 1")).not.toHaveAttribute("aria-invalid");
  });
});
