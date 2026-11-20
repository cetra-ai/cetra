import { expect, it } from "vitest";
import { parametersSummary } from "@/components/playground/run-column";

it("the parameters summary names only the set parameters", () => {
  expect(parametersSummary({ temperature: "", maxTokens: "" })).toBe("Parameters");
  expect(parametersSummary({ temperature: " 0.7 ", maxTokens: "" })).toBe("temp 0.7");
  expect(parametersSummary({ temperature: "", maxTokens: "1024" })).toBe("max 1024");
  expect(parametersSummary({ temperature: "0", maxTokens: "64" })).toBe("temp 0 · max 64");
});
