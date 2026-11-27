import { describe, expect, it } from "vitest";
import { parseDraft, parseTurnEvent } from "./types";

describe("parseTurnEvent", () => {
  it("parses each event of the turn route", () => {
    expect(
      parseTurnEvent("start", '{"turn_id":1,"trace_id":"0af7651916cd43dd8448eb211c80319c"}'),
    ).toEqual({
      event: "start",
      data: { turn_id: 1, trace_id: "0af7651916cd43dd8448eb211c80319c" },
    });
    expect(parseTurnEvent("text", '{"delta":"Hi"}')).toEqual({
      event: "text",
      data: { delta: "Hi" },
    });
    expect(
      parseTurnEvent(
        "tool_call",
        '{"id":"call_1","name":"search_traces","arguments":{"filter":"x"}}',
      ),
    ).toEqual({
      event: "tool_call",
      data: { id: "call_1", name: "search_traces", arguments: { filter: "x" } },
    });
    expect(
      parseTurnEvent(
        "tool_result",
        '{"id":"call_1","ok":true,"summary":{"size_bytes":812,"rows":3}}',
      ),
    ).toEqual({
      event: "tool_result",
      data: { id: "call_1", ok: true, summary: { size_bytes: 812, rows: 3 } },
    });
    expect(
      parseTurnEvent(
        "tool_result",
        '{"id":"call_2","ok":false,"summary":{"size_bytes":9,"error_code":"invalid"}}',
      ),
    ).toEqual({
      event: "tool_result",
      data: { id: "call_2", ok: false, summary: { size_bytes: 9, error_code: "invalid" } },
    });
    expect(
      parseTurnEvent(
        "draft",
        '{"id":"draft_call_2","kind":"filter","draft":{"view":"traces","include_eval":false,"include_experiments":false}}',
      ),
    ).toEqual({
      event: "draft",
      data: {
        id: "draft_call_2",
        kind: "filter",
        draft: { view: "traces", include_eval: false, include_experiments: false },
      },
    });
    expect(
      parseTurnEvent(
        "done",
        '{"finish_reason":"stop","usage":{"input":10,"output":2},"cost_usd":null,"steps":1}',
      ),
    ).toEqual({
      event: "done",
      data: { finish_reason: "stop", usage: { input: 10, output: 2 }, cost_usd: null, steps: 1 },
    });
    expect(
      parseTurnEvent("error", '{"class":"pause","message":"the LLM connection is busy"}'),
    ).toEqual({
      event: "error",
      data: { class: "pause", message: "the LLM connection is busy" },
    });
  });

  it("returns null for ping, an unknown name, bad JSON and a wrong shape", () => {
    expect(parseTurnEvent("ping", "{}")).toBeNull();
    expect(parseTurnEvent("mystery", "{}")).toBeNull();
    expect(parseTurnEvent("text", "{not json")).toBeNull();
    expect(parseTurnEvent("tool_result", '{"id":"call_1","ok":true}')).toBeNull();
    expect(parseTurnEvent("error", '{"class":"fatal","message":"x"}')).toBeNull();
    expect(parseTurnEvent("text", "[]")).toBeNull();
  });
});

describe("parseDraft", () => {
  it("accepts each draft shape", () => {
    expect(
      parseDraft("filter", {
        view: "traces",
        filter: "status = 'error'",
        range: "24h",
        include_eval: false,
        include_experiments: false,
        extra: "dropped",
      }),
    ).toEqual({
      kind: "filter",
      draft: {
        view: "traces",
        filter: "status = 'error'",
        range: "24h",
        include_eval: false,
        include_experiments: false,
      },
    });
    expect(
      parseDraft("sql", {
        query: "SELECT 1",
        params: { limit: 5 },
        columns: [{ name: "n", type: "integer" }],
      }),
    ).toEqual({
      kind: "sql",
      draft: { query: "SELECT 1", params: { limit: 5 }, columns: [{ name: "n", type: "integer" }] },
    });
    expect(parseDraft("dashboard", { name: "Errors", definition: { widgets: [] } })).toEqual({
      kind: "dashboard",
      draft: { name: "Errors", definition: { widgets: [] } },
    });
    expect(
      parseDraft("evaluator", {
        name: "Helpful",
        kind: "llm_judge",
        config: { connection_id: "01900000-0000-7000-8000-000000000001" },
        output_score: { name: "helpful", data_type: "boolean" },
      }),
    ).toEqual({
      kind: "evaluator",
      draft: {
        name: "Helpful",
        kind: "llm_judge",
        config: { connection_id: "01900000-0000-7000-8000-000000000001" },
        output_score: { name: "helpful", data_type: "boolean" },
      },
    });
    expect(
      parseDraft("online_rule", {
        name: "Sample",
        evaluator_id: "01900000-0000-7000-8000-000000000002",
        target: "trace",
        variable_mapping: { input: "trace.input" },
        sample_rate: 0.1,
        enabled: false,
      }),
    ).toEqual({
      kind: "online_rule",
      draft: {
        name: "Sample",
        evaluator_id: "01900000-0000-7000-8000-000000000002",
        target: "trace",
        variable_mapping: { input: "trace.input" },
        sample_rate: 0.1,
        enabled: false,
      },
    });
  });

  it("rejects a filter draft with an unknown view", () => {
    expect(
      parseDraft("filter", { view: "logs", include_eval: false, include_experiments: false }),
    ).toBeNull();
  });

  it("rejects an evaluator draft of kind webhook", () => {
    expect(
      parseDraft("evaluator", {
        name: "Hook",
        kind: "webhook",
        config: {},
        output_score: { name: "s", data_type: "numeric" },
      }),
    ).toBeNull();
  });

  it("rejects a rule draft whose sample_rate is a string", () => {
    expect(
      parseDraft("online_rule", {
        name: "Sample",
        evaluator_id: "01900000-0000-7000-8000-000000000002",
        target: "span",
        variable_mapping: {},
        sample_rate: "0.1",
        enabled: true,
      }),
    ).toBeNull();
  });
});
