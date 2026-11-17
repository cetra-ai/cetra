import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { formatScore, traceRowTone } from "@/components/trace-columns";

it("marks only failed traces as an error row", () => {
  expect(traceRowTone({ status: "error" } as Schema<"TraceSummary">)).toBe("error");
  expect(traceRowTone({ status: "ok" } as Schema<"TraceSummary">)).toBeUndefined();
  expect(traceRowTone({ status: "unset" } as Schema<"TraceSummary">)).toBeUndefined();
});

it("formats each score type", () => {
  expect(formatScore({ data_type: "numeric", value: 0.9 })).toBe("0.90");
  expect(formatScore({ data_type: "boolean", value: false })).toBe("✗");
  expect(formatScore({ data_type: "categorical", value: { label: "correct", value: 1 } })).toBe(
    "correct",
  );
  expect(formatScore({ data_type: "text", value: "Needs retry" })).toBe("Needs retry");
});
