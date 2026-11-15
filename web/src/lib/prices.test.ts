import { describe, expect, it } from "vitest";
import modelPriceFixture from "../../../fixtures/api/static/model-price.json";
import {
  blockedByRules,
  checkPriceForm,
  escapeRegex,
  formatUnitPrice,
  newTierForm,
  type PriceForm,
  type PriceRow,
  priceBody,
  priceForm,
} from "./prices";

function blank(overrides: Partial<PriceForm> = {}): PriceForm {
  return {
    modelName: "llama3.1:8b",
    matchKind: "exact",
    pattern: "",
    provider: "",
    validFrom: "",
    validFromOriginal: "",
    input: "",
    output: "",
    cacheRead: "",
    cacheWrite: "",
    reasoning: "",
    tiers: [],
    ...overrides,
  };
}

describe("formatUnitPrice", () => {
  it("formats whole, fractional and absent prices", () => {
    expect(formatUnitPrice(2.5)).toBe("$2.50");
    expect(formatUnitPrice(0)).toBe("$0");
    expect(formatUnitPrice(0.075)).toBe("$0.075");
    expect(formatUnitPrice(8)).toBe("$8");
    expect(formatUnitPrice(undefined)).toBe("—");
  });

  it("does not show a tiny non-zero price as zero", () => {
    expect(formatUnitPrice(1e-7)).toBe("$1.0e-7");
    expect(formatUnitPrice(0.0000005)).toBe("$5.0e-7");
    expect(formatUnitPrice(0.000001)).toBe("$0.000001");
  });
});

describe("escapeRegex", () => {
  it("escapes the meta characters regex::escape escapes", () => {
    expect(escapeRegex("gpt-4o")).toBe("gpt\\-4o");
    expect(escapeRegex("llama3.1:8b")).toBe("llama3\\.1:8b");
    expect(escapeRegex("a (b) [c] {d} $e ^f # g & h ~ i | j ? k * l + m \\")).toBe(
      "a \\(b\\) \\[c\\] \\{d\\} \\$e \\^f \\# g \\& h \\~ i \\| j \\? k \\* l \\+ m \\\\",
    );
  });

  it("leaves spaces and colons alone", () => {
    expect(escapeRegex("Llama 3.1 8B (local)")).toBe("Llama 3\\.1 8B \\(local\\)");
  });
});

describe("checkPriceForm", () => {
  it("asks for a name", () => {
    expect(checkPriceForm(blank({ modelName: "  ", input: "1" }))).toEqual({
      model_name: "Enter a name.",
    });
  });

  it("asks for a pattern in regex mode", () => {
    expect(checkPriceForm(blank({ matchKind: "regex", pattern: " ", input: "1" }))).toEqual({
      match_pattern: "Enter a pattern, or choose Exact name.",
    });
  });

  it("refuses a negative or non-numeric price", () => {
    expect(checkPriceForm(blank({ input: "-1", output: "1" }))).toEqual({
      price_input: "Enter a number of 0 or more.",
    });
    expect(checkPriceForm(blank({ output: "abc", input: "1" }))).toEqual({
      price_output: "Enter a number of 0 or more.",
    });
  });

  it("needs an input or an output price", () => {
    expect(checkPriceForm(blank({ cacheRead: "1" }))).toEqual({
      price_input: "Enter an input or an output price.",
    });
    expect(checkPriceForm(blank({ output: "0" }))).toEqual({});
  });

  it("checks each tier's start and prices", () => {
    const tier = newTierForm();
    expect(
      checkPriceForm(blank({ input: "1", tiers: [{ ...tier, minInputTokens: "0" }] })),
    ).toEqual({
      "tiers[0].min_input_tokens": "Enter a whole number of 1 or more.",
    });
    expect(
      checkPriceForm(blank({ input: "1", tiers: [{ ...tier, minInputTokens: "1.5" }] })),
    ).toEqual({
      "tiers[0].min_input_tokens": "Enter a whole number of 1 or more.",
    });
    expect(
      checkPriceForm(
        blank({ input: "1", tiers: [{ ...tier, minInputTokens: "128000", output: "-2" }] }),
      ),
    ).toEqual({ "tiers[0].price_output": "Enter a number of 0 or more." });
  });

  it("passes a complete form", () => {
    expect(checkPriceForm(blank({ input: "0", output: "0" }))).toEqual({});
  });
});

describe("priceBody", () => {
  it("leaves out the pattern for an exact name and blank prices", () => {
    expect(priceBody(blank({ input: "0", output: "0" }))).toEqual({
      model_name: "llama3.1:8b",
      price_input: 0,
      price_output: 0,
      tiers: [],
    });
  });

  it("sends the pattern, lowercased provider, date and tiers", () => {
    const tier = { ...newTierForm(), minInputTokens: "128000", input: "0.2" };
    expect(
      priceBody(
        blank({
          matchKind: "regex",
          pattern: " llama3.* ",
          provider: " OpenAI ",
          validFrom: "2026-03-01",
          input: "0.1",
          output: "0.2",
          tiers: [tier],
        }),
      ),
    ).toEqual({
      model_name: "llama3.1:8b",
      match_pattern: "llama3.*",
      provider: "openai",
      valid_from: "2026-03-01T00:00:00.000000000Z",
      price_input: 0.1,
      price_output: 0.2,
      tiers: [{ min_input_tokens: 128000, price_input: 0.2 }],
    });
  });

  it("round trips a row with a regex pattern", () => {
    const row = modelPriceFixture as PriceRow;
    const form = priceForm(row);
    expect(form.matchKind).toBe("regex");
    const body = priceBody(form);
    expect(body.match_pattern).toBe("gpt\\-4o");
    expect(body.price_input).toBe(row.price_input);
    expect(body.price_output).toBe(row.price_output);
    expect(body.price_cache_read).toBe(row.price_cache_read);
    expect(body.provider).toBe("openai");
    expect(body.valid_from).toBe("2026-01-01T00:00:00.000000000Z");
  });

  it("keeps the time of day of an unchanged valid_from", () => {
    const row = {
      ...(modelPriceFixture as PriceRow),
      valid_from: "2026-03-01T12:00:00.000000000Z",
    };
    const form = priceForm(row);
    expect(form.validFrom).toBe("2026-03-01");
    expect(priceBody(form).valid_from).toBe("2026-03-01T12:00:00.000000000Z");
  });

  it("sends midnight UTC when the valid_from date changes", () => {
    const row = {
      ...(modelPriceFixture as PriceRow),
      valid_from: "2026-03-01T12:00:00.000000000Z",
    };
    const form = { ...priceForm(row), validFrom: "2026-03-05" };
    expect(priceBody(form).valid_from).toBe("2026-03-05T00:00:00.000000000Z");
  });

  it("round trips a row whose pattern is its escaped name", () => {
    const row: PriceRow = {
      scope: "org",
      model_name: "llama3.1:8b",
      match_pattern: "llama3\\.1:8b",
      valid_from: "1970-01-01T00:00:00.000000000Z",
      price_input: 0,
      price_output: 0,
      tiers: [],
    };
    const form = priceForm(row);
    expect(form.matchKind).toBe("exact");
    expect(priceBody(form)).toEqual({
      model_name: "llama3.1:8b",
      price_input: 0,
      price_output: 0,
      tiers: [],
    });
  });
});

describe("blockedByRules", () => {
  it("names the budgeted rules of a 409", () => {
    const error = {
      type: "about:blank",
      title: "Conflict",
      status: 409,
      code: "conflict",
      errors: [
        {
          field: "rules.0190a",
          message: "Relevance on answers: the judge model would have no price",
        },
        { field: "rules.0190b", message: "Tone: the judge model would have no price" },
      ],
    };
    expect(blockedByRules(error)).toBe(
      "Budgeted online rules use this price: Relevance on answers, Tone.",
    );
  });

  it("keeps a colon in the rule name", () => {
    const error = {
      type: "about:blank",
      title: "Conflict",
      status: 409,
      code: "conflict",
      errors: [
        {
          field: "rules.0190c",
          message: "Tone: formal: the judge model would have no price",
        },
      ],
    };
    expect(blockedByRules(error)).toBe("Budgeted online rules use this price: Tone: formal.");
  });

  it("ignores other errors", () => {
    expect(
      blockedByRules({ type: "about:blank", title: "Conflict", status: 409, code: "conflict" }),
    ).toBeUndefined();
    expect(blockedByRules(new Error("network"))).toBeUndefined();
  });
});
