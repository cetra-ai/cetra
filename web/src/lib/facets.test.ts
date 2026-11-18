import { describe, expect, it } from "vitest";
import type { Field } from "@/lib/dsl/dsl";
import {
  carryConjunct,
  carryFilter,
  clearField,
  FACETS,
  type FacetView,
  fieldText,
  filterViewOf,
  formatBound,
  ownedConjuncts,
  PRESETS,
  presetActive,
  readRange,
  readText,
  readValues,
  roundP95,
  type Selection,
  sameField,
  togglePreset,
  withoutField,
  writeRange,
  writeText,
  writeValues,
} from "@/lib/facets";

const STATUS: Field = { column: "status" };
const NAME: Field = { column: "name" };
const TAGS: Field = { column: "tags" };
const HAS_ROOT: Field = { column: "has_root" };
const DURATION: Field = { column: "duration_ms" };
const TRACE_ID: Field = { column: "trace_id" };
const PLAN: Field = { column: "metadata", key: "plan" };
const TRICKY: Field = { column: "metadata", key: "it's plan" };

const include = (...values: string[]): Selection => ({ mode: "include", values, none: false });
const exclude = (...values: string[]): Selection => ({ mode: "exclude", values, none: false });
const none = (mode: Selection["mode"]): Selection => ({ mode, values: [], none: true });

describe("reading a values facet", () => {
  it.each([
    ["status = 'ok'", "include", ["ok"], false],
    ["status != 'ok'", "exclude", ["ok"], false],
    ["status IN ('ok', 'error')", "include", ["ok", "error"], false],
    ["status NOT IN ('ok', 'error')", "exclude", ["ok", "error"], false],
    ["status IS NULL", "include", [], true],
    ["status IS NOT NULL", "exclude", [], true],
  ])("reads %s as %s %j none=%s", (filter, mode, values, hasNone) => {
    const read = readValues(filter, STATUS, "values");
    expect(read.mixed).toBe(false);
    expect(read.selection).toEqual({ mode, values, none: hasNone });
  });

  it("reads no owned conjunct as an empty include", () => {
    expect(readValues("name = 'x'", STATUS, "values")).toEqual({
      selection: include(),
      mixed: false,
    });
  });

  it("marks two owned conjuncts as mixed", () => {
    expect(readValues("status = 'ok' AND status = 'error'", STATUS, "values").mixed).toBe(true);
  });

  it("does not own a LIKE conjunct on the Select tab", () => {
    expect(readValues("status ILIKE '%ok%'", STATUS, "values")).toEqual({
      selection: include(),
      mixed: false,
    });
  });

  it("reads a list field's contains as include", () => {
    expect(readValues("tags @> ['a', 'b']", TAGS, "list")).toEqual({
      selection: include("a", "b"),
      mixed: false,
    });
  });

  it("reads a boolean as its lower-case value", () => {
    expect(readValues("has_root = TRUE", HAS_ROOT, "bool").selection).toEqual(include("true"));
  });

  it("does not read the bare map field's conjuncts for a key", () => {
    expect(readValues("metadata['plan'] = 'pro'", { column: "metadata" }, "values")).toEqual({
      selection: include(),
      mixed: false,
    });
  });
});

describe("writing a values facet", () => {
  it.each([
    ["", include("ok"), "status = 'ok'"],
    ["", include("ok", "error"), "status IN ('ok', 'error')"],
    ["", exclude("ok"), "status != 'ok'"],
    ["", exclude("ok", "error"), "status NOT IN ('ok', 'error')"],
    ["", none("include"), "status IS NULL"],
    ["", none("exclude"), "status IS NOT NULL"],
    ["", include(), ""],
  ])("writes %j with %j as %j", (filter, selection, expected) => {
    expect(writeValues(filter, STATUS, "values", selection)).toBe(expected);
  });

  it("replaces the owned conjunct and keeps the others in order", () => {
    expect(
      writeValues(
        "name = 'x' AND status = 'ok' AND user_id = 'a'",
        STATUS,
        "values",
        include("error"),
      ),
    ).toBe("name = 'x' AND user_id = 'a' AND status = 'error'");
  });

  it("keeps a raw OR conjunct as text, parenthesised before an appended conjunct", () => {
    expect(writeValues("user_id = 'a' OR environment = 'p'", STATUS, "values", include("ok"))).toBe(
      "(user_id = 'a' OR environment = 'p') AND status = 'ok'",
    );
  });

  it("keeps LIKE conjuncts when the Select tab writes", () => {
    expect(writeValues("status ILIKE '%x%'", STATUS, "values", include("ok"))).toBe(
      "status ILIKE '%x%' AND status = 'ok'",
    );
  });

  it("writes '(none)' only when no value is selected", () => {
    expect(writeValues("", STATUS, "values", { mode: "include", values: ["ok"], none: true })).toBe(
      "status = 'ok'",
    );
  });

  it("writes a list field's contains with every value, include only", () => {
    expect(writeValues("", TAGS, "list", include("a", "b"))).toBe("tags @> ['a', 'b']");
    expect(writeValues("tags @> ['a']", TAGS, "list", include())).toBe("");
  });

  it("writes booleans as TRUE and FALSE literals", () => {
    expect(writeValues("", HAS_ROOT, "bool", include("true"))).toBe("has_root = TRUE");
    expect(writeValues("has_root = TRUE", HAS_ROOT, "bool", include("false"))).toBe(
      "has_root = FALSE",
    );
  });

  it("writes a map key with its quote escaped, and reads it back", () => {
    const text = writeValues("", TRICKY, "values", include("pro"));
    expect(text).toBe("metadata['it''s plan'] = 'pro'");
    expect(readValues(text, TRICKY, "values").selection).toEqual(include("pro"));
    expect(fieldText(TRICKY)).toBe("metadata['it''s plan']");
  });

  it("leaves a conjunct of another key alone", () => {
    expect(writeValues("metadata['plan'] = 'pro'", TRICKY, "values", include("x"))).toBe(
      "metadata['plan'] = 'pro' AND metadata['it''s plan'] = 'x'",
    );
  });
});

describe("text", () => {
  it("writes each operator through ILIKE and escapes the user's wildcards", () => {
    expect(writeText("", TRACE_ID, "text", "equals", "abc")).toBe("trace_id = 'abc'");
    expect(writeText("", TRACE_ID, "text", "contains", "ab")).toBe("trace_id ILIKE '%ab%'");
    expect(writeText("", TRACE_ID, "text", "starts", "ab")).toBe("trace_id ILIKE 'ab%'");
    expect(writeText("", TRACE_ID, "text", "not_contains", "x")).toBe("trace_id NOT ILIKE '%x%'");
    expect(writeText("", NAME, "values", "contains", "50%_off")).toBe("name ILIKE '%50\\%\\_off%'");
  });

  it("reads back what it writes", () => {
    const text = writeText("", NAME, "values", "contains", "50%_off");
    expect(readText(text, NAME, "values")).toEqual({
      op: "contains",
      text: "50%_off",
      mixed: false,
    });
    expect(readText("trace_id = 'abc'", TRACE_ID, "text")).toEqual({
      op: "equals",
      text: "abc",
      mixed: false,
    });
    expect(readText("trace_id ILIKE 'ab%'", TRACE_ID, "text")).toEqual({
      op: "starts",
      text: "ab",
      mixed: false,
    });
  });

  it("reads no text conjunct as undefined; a values Text tab ignores equality, a text facet owns it", () => {
    expect(readText("name = 'x'", NAME, "values")).toBeUndefined();
    expect(readText("name = 'x'", NAME, "text")).toEqual({
      op: "equals",
      text: "x",
      mixed: false,
    });
  });

  it("removes the owned text conjuncts on empty text, and keeps Select conjuncts", () => {
    expect(writeText("name ILIKE '%x%' AND name = 'y'", NAME, "values", "contains", "")).toBe(
      "name = 'y'",
    );
  });

  it("refuses equals for a values facet", () => {
    expect(() => writeText("", NAME, "values", "equals", "x")).toThrow();
  });
});

describe("ranges", () => {
  it("reads the bounds of a range", () => {
    expect(readRange("duration_ms >= 100 AND duration_ms < 2000", DURATION)).toEqual({
      lo: 100,
      hi: 2000,
      mixed: false,
    });
  });

  it("marks > and <= as edited text", () => {
    expect(readRange("duration_ms > 5", DURATION).mixed).toBe(true);
    expect(readRange("duration_ms <= 5", DURATION).mixed).toBe(true);
  });

  it("writes the bounds after the other conjuncts", () => {
    expect(writeRange("name = 'x'", DURATION, 100, 2000)).toBe(
      "name = 'x' AND duration_ms >= 100 AND duration_ms < 2000",
    );
    expect(writeRange("duration_ms >= 1", DURATION, undefined, 50)).toBe("duration_ms < 50");
    expect(writeRange("duration_ms >= 1", DURATION)).toBe("");
  });

  it.each([
    [0.000123456, "0.000123456"],
    [1234567, "1234570"],
    [2.5, "2.5"],
    [100, "100"],
    [0, "0"],
    [-0.5, "-0.5"],
    [1.5000001, "1.5"],
  ])("formats %d as %s", (value, expected) => {
    expect(formatBound(value)).toBe(expected);
  });
});

describe("withoutField and clearField", () => {
  it("removes every owned conjunct of the field, of any kind", () => {
    expect(withoutField("status = 'ok' AND name = 'x' AND status IN ('a')", STATUS)).toBe(
      "name = 'x'",
    );
    expect(withoutField("duration_ms >= 1 AND name = 'x'", DURATION)).toBe("name = 'x'");
    expect(withoutField("tags @> ['a'] AND name = 'x'", TAGS)).toBe("name = 'x'");
  });

  it("keeps the conjuncts of a map key when the bare map is cleared", () => {
    expect(withoutField("metadata['plan'] = 'pro'", { column: "metadata" })).toBe(
      "metadata['plan'] = 'pro'",
    );
    expect(clearField("metadata['plan'] = 'pro'", PLAN)).toBe("");
  });

  it("clears every conjunct the same way", () => {
    expect(clearField("status = 'ok' AND name = 'x'", STATUS)).toBe("name = 'x'");
  });

  it("compares fields by column and key", () => {
    expect(sameField(STATUS, { column: "status" })).toBe(true);
    expect(sameField(PLAN, { column: "metadata", key: "plan" })).toBe(true);
    expect(sameField(PLAN, { column: "metadata", key: "other" })).toBe(false);
    expect(sameField(PLAN, { column: "metadata" })).toBe(false);
  });
});

describe("owned conjuncts", () => {
  it("counts the conjuncts a values facet owns, the LIKE family included", () => {
    expect(
      ownedConjuncts("status = 'ok' AND name = 'x' AND status ILIKE '%a%'", STATUS, "values").map(
        (conjunct) => conjunct.text,
      ),
    ).toEqual(["status = 'ok'", "status ILIKE '%a%'"]);
  });

  it("owns a range's comparisons only, and a list's contains only", () => {
    expect(
      ownedConjuncts("duration_ms >= 1 AND duration_ms = 2", DURATION, "range").map(
        (conjunct) => conjunct.text,
      ),
    ).toEqual(["duration_ms >= 1"]);
    expect(ownedConjuncts("tags @> ['a'] AND tags = 'a'", TAGS, "list")).toHaveLength(1);
  });
});

describe("presets", () => {
  const traces = PRESETS.traces;
  const spans = PRESETS.spans;
  const sessions = PRESETS.sessions;
  const preset = (list: typeof traces, id: string) => {
    const found = list.find((item) => item.id === id);
    if (found === undefined) {
      throw new Error(`no preset ${id}`);
    }
    return found;
  };

  it.each<[FacetView, string, string]>([
    ["traces", "errors", "status = 'error'"],
    ["spans", "errors", "status = 'error'"],
    ["spans", "llm", "span_kind = 'llm'"],
    ["sessions", "errors", "error_count > 0"],
  ])("toggles %s %s on and off", (view, id, conjunct) => {
    const item = preset(PRESETS[view], id);
    expect(togglePreset("name = 'x'", item)).toBe(`name = 'x' AND ${conjunct}`);
    expect(presetActive(conjunct, item)).toBe(true);
    expect(togglePreset(conjunct, item)).toBe("");
    expect(presetActive("name = 'x'", item)).toBe(false);
  });

  it("recognises the error conjunct however it is spelled", () => {
    expect(presetActive("STATUS = 'error'", preset(traces, "errors"))).toBe(true);
  });

  it("sets Slow from the p95 rounded to 3 significant digits", () => {
    expect(togglePreset("", preset(traces, "slow"), 1234.5678)).toBe("duration_ms >= 1230");
    expect(togglePreset("", preset(sessions, "slow"), 1234.5678)).toBe("duration_ms >= 1230");
    expect(togglePreset("", preset(traces, "expensive"), 0.0123456)).toBe("cost_total >= 0.0123");
  });

  it("does nothing when a Slow preset has no p95", () => {
    expect(togglePreset("name = 'x'", preset(traces, "slow"))).toBe("name = 'x'");
  });

  it("is active on an owned lower bound, and turning off removes the lower bound only", () => {
    const slow = preset(spans, "slow");
    expect(presetActive("duration_ms > 5", slow)).toBe(true);
    expect(presetActive("duration_ms <= 5", slow)).toBe(false);
    expect(togglePreset("duration_ms > 5 AND duration_ms < 9 AND status = 'ok'", slow)).toBe(
      "duration_ms < 9 AND status = 'ok'",
    );
  });

  it("rounds a p95 to 3 significant digits", () => {
    expect(roundP95(1234.5)).toBe(1230);
    expect(roundP95(0.5)).toBe(0.5);
  });
});

describe("carrying filters between lists", () => {
  it("keeps every conjunct between the traces and the users lists", () => {
    const filter = "name = 'x' AND user_id = 'a'";
    expect(carryFilter(filter, "traces", "users")).toEqual({
      kept: "name = 'x' AND user_id = 'a'",
      dropped: 0,
    });
    expect(filterViewOf("users")).toBe("traces");
  });

  it("keeps the carried identifiers across the traces and spans views", () => {
    expect(carryConjunct("user_id = 'a'", "traces", "spans")).toBe(true);
    expect(carryConjunct("name = 'x'", "traces", "spans")).toBe(false);
    expect(carryConjunct("score('x') > 1", "traces", "spans")).toBe(false);
    expect(carryConjunct("user_id = 'a' OR environment = 'p'", "traces", "spans")).toBe(true);
    expect(carryConjunct("user_id = 'a' OR name = 'b'", "traces", "spans")).toBe(false);
  });

  it("keeps only user_id and tags for the sessions list", () => {
    expect(
      carryFilter("session_id = 's' AND user_id = 'a' AND tags @> ['x']", "traces", "sessions"),
    ).toEqual({ kept: "user_id = 'a' AND tags @> ['x']", dropped: 1 });
    expect(carryFilter("user_id = 'a' OR environment = 'p'", "traces", "spans")).toEqual({
      kept: "user_id = 'a' OR environment = 'p'",
      dropped: 0,
    });
  });
});

describe("facet config", () => {
  it.each<FacetView>(["traces", "spans", "sessions"])(
    "gives the %s list unique section ids",
    (view) => {
      const ids = FACETS[view].map((facet) => facet.id);
      expect(new Set(ids).size).toBe(ids.length);
    },
  );

  it("opens the default sections of each list", () => {
    expect(FACETS.traces.filter((facet) => facet.defaultOpen).map((facet) => facet.id)).toEqual([
      "status",
      "name",
      "environment",
      "duration_ms",
    ]);
  });
});

describe("review fixes", () => {
  it("round-trips a value with a quote through the values facet", () => {
    const filter = "name = 'it''s'";
    const { selection, mixed } = readValues(filter, NAME, "values");
    expect(mixed).toBe(false);
    expect(selection).toEqual({ mode: "include", values: ["it's"], none: false });
    expect(writeValues(filter, NAME, "values", selection)).toBe(filter);
  });

  it("drops a raw conjunct that names a field the target list does not carry", () => {
    const filter = "user_id = 'a' OR name = 'x'";
    expect(carryFilter(filter, "sessions", "traces")).toEqual({ kept: "", dropped: 1 });
    expect(carryFilter(filter, "sessions", "spans")).toEqual({ kept: "", dropped: 1 });
    expect(carryFilter("user_id = 'a' OR user_id = 'b'", "sessions", "spans")).toEqual({
      kept: "user_id = 'a' OR user_id = 'b'",
      dropped: 0,
    });
  });

  it("edits as text a case-sensitive LIKE and an unescaped wildcard", () => {
    expect(readText("name LIKE 'x%'", NAME, "values")).toMatchObject({ mixed: true });
    expect(readText("name ILIKE '%a_b%'", NAME, "values")).toMatchObject({ mixed: true });
    expect(readText("name ILIKE '%a%b%'", NAME, "values")).toMatchObject({ mixed: true });
    expect(readText("name ILIKE '%a\\_b%'", NAME, "values")).toEqual({
      op: "contains",
      text: "a_b",
      mixed: false,
    });
  });

  it("writes a bound below 1e-9 as 0", () => {
    expect(formatBound(1e-12)).toBe("0");
    expect(formatBound(-1e-12)).toBe("0");
    expect(formatBound(0.5)).toBe("0.5");
  });

  it("never throws on a non-finite bound, and writes it as 0", () => {
    expect(formatBound(Number.NaN)).toBe("0");
    expect(formatBound(Number.POSITIVE_INFINITY)).toBe("0");
    expect(formatBound(Number.NEGATIVE_INFINITY)).toBe("0");
  });
});
