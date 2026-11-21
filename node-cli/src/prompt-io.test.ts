import { PassThrough, Writable } from "node:stream";
import { describe, expect, it } from "vitest";
import { ask, yes } from "./prompt-io.js";

/** A writable that discards the prompt text. */
function sink(): Writable {
  return new Writable({
    write(_chunk, _encoding, callback) {
      callback();
    },
  });
}

describe("yes", () => {
  it.each([
    ["", true, true],
    ["", false, false],
    ["Y", false, true],
    ["yes", false, true],
    ["  YES  ", false, true],
    ["n", true, false],
    ["no", true, false],
    ["maybe", true, false],
  ])("reads %j with default %s as %s", (answer, defaultYes, expected) => {
    expect(yes(answer, defaultYes)).toBe(expected);
  });
});

describe("ask", () => {
  it("returns the trimmed answer", async () => {
    const input = new PassThrough();
    const pending = ask("Name? ", "fallback", input, sink());
    input.write("  x  \n");
    expect(await pending).toBe("x");
  });

  it("returns the fallback for an empty line", async () => {
    const input = new PassThrough();
    const pending = ask("Name? ", "fallback", input, sink());
    input.write("\n");
    expect(await pending).toBe("fallback");
  });

  it("closes its interface, so the input can end", async () => {
    const input = new PassThrough();
    const pending = ask("Name? ", "fallback", input, sink());
    input.write("x\n");
    await pending;
    expect(input.listenerCount("data")).toBe(0);
    expect(input.listenerCount("keypress")).toBe(0);
  });
});
