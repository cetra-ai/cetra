import { context, diag, propagation, ROOT_CONTEXT, trace } from "@opentelemetry/api";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { CONTEXT_KEY, currentValues, experimentContext, withContext } from "./context.js";
import { Prompt } from "./prompt.js";
import { resetForTests } from "./runtime.js";
import { finished, startMemory } from "./testing.test-helpers.js";

beforeEach(async () => {
  await resetForTests();
});

afterEach(async () => {
  await resetForTests();
});

describe("withContext", () => {
  it("stamps the values on spans inside the block, children included", async () => {
    const { exporter, runtime } = startMemory({ environment: "staging" });
    withContext(
      { sessionId: "s-1", userId: "u-1", tags: ["beta"], metadata: { plan: "pro" } },
      () => {
        runtime.tracer.startActiveSpan("parent", (parent) => {
          runtime.tracer.startSpan("child").end();
          parent.end();
        });
      },
    );
    const spans = await finished(exporter);
    expect(spans.map((span) => span.name).sort()).toEqual(["child", "parent"]);
    for (const span of spans) {
      expect(span.attributes["cetra.session.id"]).toBe("s-1");
      expect(span.attributes["cetra.user.id"]).toBe("u-1");
      expect(span.attributes["cetra.tags"]).toEqual(["beta"]);
      expect(span.attributes["cetra.metadata.plan"]).toBe("pro");
      expect(span.attributes["cetra.environment"]).toBe("staging");
    }
  });

  it("stamps spans of another tracer of our provider", async () => {
    const { exporter, runtime } = startMemory();
    const thirdParty = runtime.provider.getTracer("thirdparty");
    withContext({ sessionId: "s-1" }, () => {
      thirdParty.startSpan("ext").end();
    });
    const [span] = await finished(exporter);
    expect(span?.name).toBe("ext");
    expect(span?.attributes["cetra.session.id"]).toBe("s-1");
  });

  it("merges nested blocks and restores the outer values on exit", async () => {
    const { exporter, runtime } = startMemory();
    withContext(
      {
        sessionId: "outer",
        userId: "u-1",
        tags: ["a", "b"],
        metadata: { k: "outer", onlyOuter: 1 },
      },
      () => {
        withContext({ sessionId: "inner", tags: ["b", "c"], metadata: { k: "inner" } }, () => {
          runtime.tracer.startSpan("nested").end();
        });
        runtime.tracer.startSpan("outer-span").end();
      },
    );
    runtime.tracer.startSpan("after").end();
    const spans = await finished(exporter);
    const byName = new Map(spans.map((span) => [span.name, span]));
    const nested = byName.get("nested")?.attributes ?? {};
    expect(nested["cetra.session.id"]).toBe("inner");
    expect(nested["cetra.user.id"]).toBe("u-1");
    expect(nested["cetra.tags"]).toEqual(["a", "b", "c"]);
    expect(nested["cetra.metadata.k"]).toBe("inner");
    expect(nested["cetra.metadata.onlyOuter"]).toBe(1);
    const outerSpan = byName.get("outer-span")?.attributes ?? {};
    expect(outerSpan["cetra.session.id"]).toBe("outer");
    expect(outerSpan["cetra.tags"]).toEqual(["a", "b"]);
    expect(outerSpan["cetra.metadata.k"]).toBe("outer");
    const after = byName.get("after")?.attributes ?? {};
    expect(after["cetra.session.id"]).toBeUndefined();
    expect(after["cetra.metadata.k"]).toBeUndefined();
    expect(currentValues().sessionId).toBeUndefined();
  });

  it("isolates two concurrent async flows", async () => {
    const { exporter, runtime } = startMemory();
    const pause = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));
    await Promise.all([
      withContext({ sessionId: "one" }, async () => {
        await pause(15);
        runtime.tracer.startSpan("slow").end();
      }),
      withContext({ sessionId: "two" }, async () => {
        await pause(1);
        runtime.tracer.startSpan("fast").end();
      }),
    ]);
    const spans = await finished(exporter);
    const byName = new Map(spans.map((span) => [span.name, span]));
    expect(byName.get("slow")?.attributes["cetra.session.id"]).toBe("one");
    expect(byName.get("fast")?.attributes["cetra.session.id"]).toBe("two");
  });

  it("sets only the two baggage keys, only with propagate", () => {
    startMemory();
    // Without propagate the baggage is untouched.
    withContext({ sessionId: "s-1", userId: "u-1", tags: ["t"] }, () => {
      expect(propagation.getBaggage(context.active())).toBeUndefined();
    });
    // With propagate the existing baggage is kept and only our two keys added.
    const base = propagation.createBaggage().setEntry("correlation-id", { value: "c-1" });
    context.with(propagation.setBaggage(context.active(), base), () => {
      withContext(
        { sessionId: "s-1", userId: "u-1", tags: ["t"], metadata: { m: 1 }, propagate: true },
        () => {
          const entries =
            propagation
              .getBaggage(context.active())
              ?.getAllEntries()
              .sort(([a], [b]) => (a < b ? -1 : 1)) ?? [];
          expect(entries).toEqual([
            ["cetra.session.id", { value: "s-1" }],
            ["cetra.user.id", { value: "u-1" }],
            ["correlation-id", { value: "c-1" }],
          ]);
        },
      );
    });
  });

  it("reads the session and user from baggage alone, below the context values", async () => {
    const { exporter, runtime } = startMemory();
    const baggage = propagation
      .createBaggage()
      .setEntry("cetra.session.id", { value: "bag-s" })
      .setEntry("cetra.user.id", { value: "bag-u" });
    context.with(propagation.setBaggage(ROOT_CONTEXT, baggage), () => {
      runtime.tracer.startSpan("from-baggage").end();
      withContext({ sessionId: "ctx-s" }, () => {
        runtime.tracer.startSpan("wins").end();
      });
    });
    const spans = await finished(exporter);
    const byName = new Map(spans.map((span) => [span.name, span]));
    expect(byName.get("from-baggage")?.attributes["cetra.session.id"]).toBe("bag-s");
    expect(byName.get("from-baggage")?.attributes["cetra.user.id"]).toBe("bag-u");
    expect(byName.get("wins")?.attributes["cetra.session.id"]).toBe("ctx-s");
  });

  it("drops invalid options silently, keeping a string tags as one tag", async () => {
    const { exporter, runtime } = startMemory();
    withContext(
      {
        sessionId: 42 as unknown as string,
        userId: null as unknown as string,
        tags: [1, "b", true] as unknown as readonly string[],
        metadata: "nope" as unknown as Readonly<Record<string, unknown>>,
      },
      () => {
        runtime.tracer.startSpan("junk").end();
      },
    );
    withContext({ tags: "solo" as unknown as readonly string[] }, () => {
      runtime.tracer.startSpan("one-tag").end();
    });
    const spans = await finished(exporter);
    const byName = new Map(spans.map((span) => [span.name, span]));
    const junk = byName.get("junk")?.attributes ?? {};
    expect(junk["cetra.session.id"]).toBeUndefined();
    expect(junk["cetra.user.id"]).toBeUndefined();
    expect(junk["cetra.tags"]).toEqual(["b"]);
    expect(junk["cetra.metadata.m"]).toBeUndefined();
    expect(byName.get("one-tag")?.attributes["cetra.tags"]).toEqual(["solo"]);
  });

  it("runs fn without a runtime and returns its value, a promise included", () => {
    expect(withContext({ sessionId: "s-1" }, () => "ok")).toBe("ok");
    expect(withContext({}, () => 42)).toBe(42);
    const promise = Promise.resolve(7);
    expect(withContext({}, () => promise)).toBe(promise);
    expect(() =>
      withContext({}, () => {
        throw new Error("boom");
      }),
    ).toThrow("boom");
  });
});

describe("metadata attributes", () => {
  it("writes scalar types and JSON strings, skipping bad keys", async () => {
    const { exporter, runtime } = startMemory();
    const debug = vi.spyOn(diag, "debug").mockImplementation(() => {});
    try {
      withContext(
        {
          metadata: {
            seats: 3,
            ratio: 0.5,
            flag: true,
            obj: { a: 1 },
            big: 2 ** 60,
            "bad key!": 1,
          },
        },
        () => {
          runtime.tracer.startSpan("md").end();
        },
      );
      const [span] = await finished(exporter);
      const attributes = span?.attributes ?? {};
      expect(attributes["cetra.metadata.seats"]).toBe(3);
      expect(attributes["cetra.metadata.ratio"]).toBe(0.5);
      expect(attributes["cetra.metadata.flag"]).toBe(true);
      expect(attributes["cetra.metadata.obj"]).toBe('{"a":1}');
      // Not a safe integer, so a JSON string; JS prints the shortest
      // round-trip decimal (Python would print the exact digits).
      expect(attributes["cetra.metadata.big"]).toBe("1152921504606847000");
      expect(attributes["cetra.metadata.bad key!"]).toBeUndefined();
      // Once per key, length only, never the key itself.
      expect(debug).toHaveBeenCalledWith("cetra: metadata key of length 8 skipped");
      expect(debug).toHaveBeenCalledTimes(1);
    } finally {
      debug.mockRestore();
    }
  });

  it("masks metadata values when the block is entered", async () => {
    const { exporter, runtime } = startMemory({
      mask: (value) => (typeof value === "string" ? value.toUpperCase() : value),
    });
    withContext({ metadata: { note: "quiet" } }, () => {
      runtime.tracer.startSpan("m").end();
    });
    const [span] = await finished(exporter);
    expect(span?.attributes["cetra.metadata.note"]).toBe("QUIET");
  });
});

describe("experiment context", () => {
  it("experimentContextHasOnlyTheLink", () => {
    startMemory();
    withContext({ sessionId: "s", metadata: { a: 1 } }, () => {
      context.with(experimentContext("e1", "i1"), () => {
        expect(currentValues()).toMatchObject({
          sessionId: undefined,
          userId: undefined,
          tags: [],
          metadata: {},
          experiment: { experimentId: "e1", datasetItemId: "i1" },
        });
        expect(trace.getActiveSpan()).toBeUndefined();
      });
      expect(currentValues().sessionId).toBe("s");
    });
  });

  it("withContextInsideAnExperimentKeepsTheLink", () => {
    startMemory();
    context.with(experimentContext("e1", "i1"), () => {
      withContext({ userId: "u-1" }, () => {
        expect(currentValues()).toMatchObject({
          userId: "u-1",
          experiment: { experimentId: "e1", datasetItemId: "i1" },
        });
      });
    });
  });
});

describe("the context key", () => {
  it("never throws on garbage under the key", async () => {
    const { exporter, runtime } = startMemory();
    context.with(context.active().setValue(CONTEXT_KEY, "garbage"), () => {
      runtime.tracer.startSpan("g1").end();
    });
    context.with(
      context.active().setValue(CONTEXT_KEY, { sessionId: 42, tags: "x", metadata: [1] }),
      () => {
        runtime.tracer.startSpan("g2").end();
      },
    );
    const spans = await finished(exporter);
    const byName = new Map(spans.map((span) => [span.name, span]));
    const g2 = byName.get("g2")?.attributes ?? {};
    expect(byName.has("g1")).toBe(true);
    expect(g2["cetra.session.id"]).toBeUndefined();
    expect(g2["cetra.tags"]).toBeUndefined();
    expect(g2["cetra.metadata.length"]).toBeUndefined();
  });
});

describe("withContext prompt", () => {
  const prompt = (name: string, version: number | undefined): Prompt =>
    new Prompt({
      name,
      version,
      config: {},
      labels: [],
      isFallback: version === undefined,
      template: "Hi {{ name }}",
    });

  it("stamps the prompt name and version on spans inside the block, children included", async () => {
    const { exporter, runtime } = startMemory();
    withContext({ prompt: prompt("support-reply", 3) }, () => {
      runtime.tracer.startActiveSpan("parent", (parent) => {
        runtime.tracer.startSpan("child").end();
        parent.end();
      });
    });
    const spans = await finished(exporter);
    expect(spans).toHaveLength(2);
    for (const span of spans) {
      expect(span.attributes["cetra.prompt.name"]).toBe("support-reply");
      expect(span.attributes["cetra.prompt.version"]).toBe(3);
    }
  });

  it("keeps the outer prompt in a nested block without one, and the inner prompt wins", async () => {
    const { exporter, runtime } = startMemory();
    withContext({ prompt: prompt("outer", 1) }, () => {
      withContext({ userId: "u-1" }, () => {
        runtime.tracer.startSpan("kept").end();
      });
      withContext({ prompt: prompt("inner", 2) }, () => {
        runtime.tracer.startSpan("inner").end();
      });
    });
    runtime.tracer.startSpan("outside").end();
    const spans = await finished(exporter);
    const named = (name: string) => spans.find((span) => span.name === name);
    expect(named("kept")?.attributes["cetra.prompt.name"]).toBe("outer");
    expect(named("inner")?.attributes["cetra.prompt.name"]).toBe("inner");
    expect(named("inner")?.attributes["cetra.prompt.version"]).toBe(2);
    expect(named("outside")?.attributes["cetra.prompt.name"]).toBeUndefined();
  });

  it("stamps the name only for a fallback", async () => {
    const { exporter, runtime } = startMemory();
    withContext({ prompt: prompt("fallback-only", undefined) }, () => {
      runtime.tracer.startSpan("span").end();
    });
    const [span] = await finished(exporter);
    expect(span?.attributes["cetra.prompt.name"]).toBe("fallback-only");
    expect(span?.attributes["cetra.prompt.version"]).toBeUndefined();
  });

  it("drops a value that is not a Prompt", async () => {
    const { exporter, runtime } = startMemory();
    withContext({ prompt: "support-reply" as unknown as Prompt }, () => {
      runtime.tracer.startSpan("span").end();
    });
    const [span] = await finished(exporter);
    expect(span?.attributes["cetra.prompt.name"]).toBeUndefined();
    expect(currentValues().prompt).toBeUndefined();
  });
});
