/**
 * The tested examples of the README and the docs site (T13, D-W-6): every
 * function marked `// example:<name>` in examples.test-helpers.ts runs against
 * `startMemory`, and its body (dedented) must be byte-identical to every block
 * preceded by an `example:<name>` marker in README.md or in the site pages.
 */

import { existsSync, readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  aiTelemetryExample,
  quickStart,
  scoreExample,
  withContextExample,
  withLlmSpanExample,
} from "./examples.test-helpers.js";
import { init, resetForTests } from "./runtime.js";
import { finished, type MemoryRuntime, startMemory } from "./testing.test-helpers.js";
import type { FetchLike } from "./web.js";

const README = new URL("../README.md", import.meta.url);
const SITE = new URL("../../../website/src/content/docs/", import.meta.url);
const EXAMPLES = new URL("./examples.test-helpers.ts", import.meta.url);
/** A UUIDv7, which is what `score()` mints (D-F-2). */
const UUID7 = /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

interface Example {
  readonly run: () => unknown;
  readonly assert: (memory: MemoryRuntime, result: unknown) => Promise<void> | void;
}

const EXAMPLES_BY_NAME: Record<string, Example> = {
  "quick-start": {
    run: quickStart,
    assert: async (memory) => {
      const [span] = await finished(memory.exporter);
      expect(span?.name).toBe("search_docs");
      expect(span?.attributes["cetra.span.kind"]).toBe("tool");
      expect(span?.attributes["cetra.input"]).toBe('{"query":"pricing"}');
      expect(span?.attributes["cetra.output"]).toBe("results for pricing");
    },
  },
  "with-llm-span": {
    run: withLlmSpanExample,
    assert: async (memory) => {
      const [span] = await finished(memory.exporter);
      expect(span?.name).toBe("chat");
      expect(span?.attributes["gen_ai.provider.name"]).toBe("openai");
      expect(span?.attributes["gen_ai.request.model"]).toBe("gpt-4o-mini");
      expect(span?.attributes["gen_ai.usage.input_tokens"]).toBe(12);
      expect(span?.attributes["gen_ai.usage.output_tokens"]).toBe(3);
    },
  },
  "with-context": {
    run: withContextExample,
    assert: async (memory) => {
      const [span] = await finished(memory.exporter);
      expect(span?.name).toBe("turn");
      expect(span?.attributes["cetra.session.id"]).toBe("chat-42");
      expect(span?.attributes["cetra.user.id"]).toBe("u-17");
      expect(span?.attributes["cetra.tags"]).toEqual(["beta"]);
    },
  },
  score: {
    run: scoreExample,
    // Scores go over REST, never as spans; the queue delivers in the
    // background (its send is what shutdown flushes afterwards).
    assert: (memory, result) => {
      expect(result).toMatch(UUID7);
      expect(memory.exporter.getFinishedSpans()).toEqual([]);
    },
  },
  "ai-telemetry": {
    run: aiTelemetryExample,
    // The tracer `aiTelemetry()` hands out is the cetra provider's (scope
    // `ai`), so AI SDK spans flow to cetra. `sessionId` in the options rides
    // in the AI SDK's telemetry metadata; the `cetra.session.id` attribute
    // comes from `withContext`, as in the with-context example.
    assert: async (memory) => {
      const [span] = await finished(memory.exporter);
      expect(span?.name).toBe("ai.chat");
      expect(span?.instrumentationScope.name).toBe("ai");
    },
  },
};

describe("documented examples", () => {
  let memory: MemoryRuntime;

  beforeEach(async () => {
    await resetForTests();
    // The examples call init() themselves; init is once per process (D-F-15),
    // so the harness claims the first call and keeps the memory runtime. The
    // score example queues one score whose cleanup flush would otherwise POST
    // for real (to the refused 127.0.0.1:9) and retry the refusals: the stub
    // answers 200 without touching the network.
    init({ enabled: false });
    memory = startMemory(undefined, undefined, { fetch: okFetch });
  });

  afterEach(async () => {
    await resetForTests();
  }, 30_000);

  for (const [name, example] of Object.entries(EXAMPLES_BY_NAME)) {
    it(name, async () => {
      const result = await example.run();
      await example.assert(memory, result);
    }, 30_000);
  }

  it("matches every marked block byte for byte", () => {
    const blocks = [...pages()].flatMap(([path, text]) => markedBlocks(path, text));
    const bodies = markedBodies(readFileSync(EXAMPLES, "utf8"));
    expect(Object.keys(EXAMPLES_BY_NAME).sort()).toEqual([...bodies.keys()].sort());
    expect([...new Set(blocks.map((block) => block.name))].sort()).toEqual(
      [...bodies.keys()].sort(),
    );
    for (const block of blocks) {
      expect(block.code, `${block.path} block "${block.name}"`).toBe(bodies.get(block.name));
    }
  });
});

/** The README and every Markdown or MDX page of the docs site, by path. */
function pages(): Map<string, string> {
  const found = new Map<string, string>([[fileURLToPath(README), readFileSync(README, "utf8")]]);
  const site = fileURLToPath(SITE);
  if (existsSync(site)) {
    for (const entry of readdirSync(site, { recursive: true, encoding: "utf8" }).sort()) {
      if (/\.mdx?$/.test(entry)) {
        found.set(`${site}${entry}`, readFileSync(`${site}${entry}`, "utf8"));
      }
    }
  }
  return found;
}

/** A `fetch` that answers 200 to everything; no request leaves the process. */
const okFetch: FetchLike = () =>
  Promise.resolve({ status: 200, ok: true, headers: { get: () => null } });

interface Block {
  readonly path: string;
  readonly name: string;
  readonly code: string;
}

/** The `ts` fenced blocks preceded by an `example:<name>` HTML or MDX comment. */
function markedBlocks(path: string, markdown: string): Block[] {
  const blocks: Block[] = [];
  const lines = markdown.split("\n");
  for (let i = 0; i < lines.length; i++) {
    const marker = /^(?:<!-- example:([a-z0-9-]+) -->|\{\/\* example:([a-z0-9-]+) \*\/\})$/.exec(
      lines[i] ?? "",
    );
    if (marker === null) {
      continue;
    }
    const name = marker[1] ?? marker[2] ?? "";
    if (lines[i + 1] !== "```ts") {
      throw new Error(`${path}: no ts fence after the "${name}" marker`);
    }
    const close = lines.indexOf("```", i + 2);
    if (close === -1) {
      throw new Error(`${path}: unterminated fence after the "${name}" marker`);
    }
    blocks.push({ path, name, code: lines.slice(i + 2, close).join("\n") });
  }
  return blocks;
}

/** The bodies of the functions marked `// example:<name>`, dedented. */
function markedBodies(source: string): Map<string, string> {
  const bodies = new Map<string, string>();
  const lines = source.split("\n");
  for (let i = 0; i < lines.length; i++) {
    const marker = /^\/\/ example:([a-z0-9-]+)$/.exec(lines[i] ?? "");
    if (marker === null) {
      continue;
    }
    const open = lines.findIndex((line, index) => index > i && line.startsWith("export "));
    if (open === -1) {
      throw new Error(`no exported function after the "${marker[1]}" marker`);
    }
    // Biome keeps the top-level closing brace at column 0; inner braces are
    // indented, so the first line that is exactly "}" closes the function.
    const close = lines.indexOf("}", open + 1);
    if (close === -1) {
      throw new Error(`unterminated function after the "${marker[1]}" marker`);
    }
    bodies.set(marker[1] ?? "", dedent(lines.slice(open + 1, close)));
  }
  return bodies;
}

/** Strip the body's common leading indentation (blank lines stay blank). */
function dedent(lines: string[]): string {
  const widths = lines
    .filter((line) => line.trim() !== "")
    .map((line) => line.length - line.trimStart().length);
  const width = widths.length > 0 ? Math.min(...widths) : 0;
  return lines.map((line) => line.slice(width)).join("\n");
}
