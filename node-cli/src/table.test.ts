import { describe, expect, it } from "vitest";
import { renderTable } from "./table.js";

describe("renderTable", () => {
  it("aligns numeric columns to the right and trims trailing spaces", () => {
    const text = renderTable(
      [
        { name: "name", type: "string" },
        { name: "n", type: "integer" },
      ],
      [
        { name: "chat", n: 15 },
        { name: "summarize", n: 7 },
      ],
    );
    expect(text).toBe("name        n\n---------  --\nchat       15\nsummarize   7\n");
  });

  it("aligns a mixed column to the left", () => {
    const text = renderTable([{ name: "m", type: "string" }], [{ m: 1 }, { m: "a" }]);
    expect(text).toBe("m\n-\n1\na\n");
  });

  it("writes NULL for a null cell and for a missing key", () => {
    const text = renderTable(
      [
        { name: "a", type: "integer" },
        { name: "b", type: "string" },
        { name: "c", type: "string" },
      ],
      [
        { a: 1, b: null, c: "x" },
        { a: 2, c: "y" },
      ],
    );
    expect(text).toBe("a  b     c\n-  ----  -\n1  NULL  x\n2  NULL  y\n");
  });

  it("writes booleans, arrays and objects as the Rust CLI does", () => {
    const text = renderTable(
      [
        { name: "t", type: "boolean" },
        { name: "l", type: "list" },
        { name: "s", type: "json" },
      ],
      [{ t: true, l: [1, "a"], s: { k: { n: [1, 2] } } }],
    );
    // Widths: t 4 (the "true" cell), l 7 (`[1,"a"]`), s 17 (`{"k":{"n":[1,2]}}`).
    expect(text).toBe(
      [
        `t${" ".repeat(5)}l${" ".repeat(8)}s`,
        `${"-".repeat(4)}  ${"-".repeat(7)}  ${"-".repeat(17)}`,
        'true  [1,"a"]  {"k":{"n":[1,2]}}',
        "",
      ].join("\n"),
    );
  });

  it("cuts a cell over 60 characters to 59 and an ellipsis", () => {
    const long = "x".repeat(70);
    const text = renderTable([{ name: "h", type: "string" }], [{ h: long }]);
    expect(text).toBe(`h\n${"-".repeat(60)}\n${"x".repeat(59)}…\n`);
  });

  it("counts characters, not UTF-16 units, when it cuts", () => {
    const text = renderTable([{ name: "h", type: "string" }], [{ h: "😀".repeat(61) }]);
    expect(text).toBe(`h\n${"-".repeat(60)}\n${"😀".repeat(59)}…\n`);
  });

  it("cuts a long header the same way", () => {
    const header = "h".repeat(61);
    const text = renderTable([{ name: header, type: "string" }], []);
    expect(text).toBe(`${"h".repeat(59)}…\n${"-".repeat(60)}\n`);
  });

  it("replaces control characters with spaces in cells and headers", () => {
    expect(renderTable([{ name: "h", type: "string" }], [{ h: "a\tb\nc" }])).toBe(
      "h\n-----\na b c\n",
    );
    expect(renderTable([{ name: "x\u001by", type: "string" }], [])).toBe("x y\n---\n");
  });

  it("prints the header and the rule for a table with no rows", () => {
    expect(renderTable([{ name: "h", type: "string" }], [])).toBe("h\n-\n");
  });

  it("returns the empty string when there are no columns", () => {
    expect(renderTable([], [])).toBe("");
    expect(renderTable([], [{ a: 1 }])).toBe("");
  });
});
