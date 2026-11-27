import { expect, it } from "vitest";
import { formatSql } from "@/lib/sql/format-sql";

it("upper-cases keywords and breaks the clauses onto lines", () => {
  expect(formatSql("select trace_id, name from traces where name = 'a' limit 10")).toBe(
    "SELECT\n  trace_id,\n  name\nFROM\n  traces\nWHERE\n  name = 'a'\nLIMIT\n  10",
  );
});

it("keeps query placeholders byte for byte", () => {
  const formatted = formatSql(
    "select * from traces where start_time > {since:Timestamp} limit {limit:Int64}",
  );
  expect(formatted).toContain("{since:Timestamp}");
  expect(formatted).toContain("{limit:Int64}");
});

it("keeps SQL syntax intact", () => {
  const formatted = formatSql(
    "select approx_percentile_cont(duration_ms, 0.5), metadata['key'], date_trunc('day', start_time), x::BIGINT from spans",
    ["approx_percentile_cont"],
  );
  expect(formatted).toContain("approx_percentile_cont(duration_ms, 0.5)");
  expect(formatted).toContain("metadata['key']");
  expect(formatted).toContain("date_trunc('day', start_time)");
  expect(formatted).toContain("x::BIGINT");
});

it("keeps a function call tight only when its name is passed", () => {
  const query = "select approx_percentile_cont(duration_ms, 0.5) from spans";
  expect(formatSql(query)).toContain("approx_percentile_cont (duration_ms, 0.5)");
  expect(formatSql(query, ["approx_percentile_cont"])).toContain(
    "approx_percentile_cont(duration_ms, 0.5)",
  );
  expect(formatSql(query, ["Approx_Percentile_Cont"])).toContain(
    "approx_percentile_cont(duration_ms, 0.5)",
  );
});

it("formats a join with aliases", () => {
  const formatted = formatSql(
    "select t.trace_id, s.span_id from traces t join spans s on s.trace_id = t.trace_id where t.name = {name:String}",
  );
  expect(formatted).toContain("FROM\n  traces t\n  JOIN spans s ON s.trace_id = t.trace_id");
  expect(formatted).toContain("{name:String}");
});

it("returns text that does not parse unchanged", () => {
  expect(formatSql("select (1")).toBe("select (1");
});

it("returns blank text unchanged", () => {
  expect(formatSql("")).toBe("");
  expect(formatSql("  \n ")).toBe("  \n ");
});

it("is idempotent", () => {
  const once = formatSql(
    "select approx_percentile_cont(d, 0.5), {limit:Int64} from traces t join spans s on s.trace_id = t.trace_id",
    ["approx_percentile_cont"],
  );
  expect(formatSql(once, ["approx_percentile_cont"])).toBe(once);
});
