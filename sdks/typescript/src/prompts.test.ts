import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError, Client, type RestFetch } from "./client.js";
import { Prompt, PromptNotAvailable } from "./prompt.js";
import { getPrompt, PromptCache, parsePrompt, setPromptCacheForTests } from "./prompts.js";
import { BASE, FakeRest, inOrder } from "./rest.test-helpers.js";
import { resetForTests } from "./runtime.js";
import { PromptCompileError } from "./template.js";
import { finished, startMemory } from "./testing.test-helpers.js";

type Response = Awaited<ReturnType<RestFetch>>;

const NAME = "support-reply";
let clock = 1_000_000;

beforeEach(async () => {
  await resetForTests();
  clock = 1_000_000;
});

afterEach(async () => {
  vi.restoreAllMocks();
  await resetForTests();
});

/** A text prompt body, as the resolve route answers it. */
function body(version: number, template = "Hello {{ name }}"): Record<string, unknown> {
  return {
    name: NAME,
    type: "text",
    version,
    template,
    config: { model: "m" },
    labels: ["production"],
    variables: ["name"],
    placeholders: [],
    commit_message: null,
    created_at: "2026-11-01T00:00:00Z",
  };
}

/** A chat prompt body with one placeholder. */
const CHAT_BODY = {
  name: NAME,
  type: "chat",
  version: 3,
  template: [
    { role: "system", content: "You help {{ product }}." },
    { type: "placeholder", name: "history" },
    { role: "user", content: "{{ question }}" },
  ],
  config: {},
  labels: ["production"],
  variables: ["product", "question"],
  placeholders: ["history"],
  commit_message: null,
  created_at: "2026-11-01T00:00:00Z",
};

/**
 * Starts the in-memory runtime and installs a prompt cache over `fetch` and
 * `sleep` as the process cache, on the test clock.
 */
function install(
  fetch: RestFetch,
  sleep: (ms: number) => Promise<void> = async () => {},
): ReturnType<typeof startMemory> & { cache: PromptCache } {
  const memory = startMemory({ baseUrl: "http://cetra.test" });
  const cache = new PromptCache({ fetch, sleep, now: () => clock });
  setPromptCacheForTests(cache);
  return { ...memory, cache };
}

/** A response of the fetch seam. */
function respond(status: number, json?: unknown): Response {
  return {
    status,
    ok: status >= 200 && status < 300,
    headers: { get: () => null },
    text: async () => (json === undefined ? "" : JSON.stringify(json)),
  };
}

/** The error a promise or a function raised. */
async function rejectionOf(promise: Promise<unknown>): Promise<unknown> {
  return promise.then(
    () => {
      throw new Error("expected a rejection");
    },
    (error: unknown) => error,
  );
}

function thrownBy(fn: () => unknown): unknown {
  try {
    fn();
  } catch (error) {
    return error;
  }
  throw new Error("expected a throw");
}

describe("getPrompt caching", () => {
  it("fetches a label once and serves it from the cache", async () => {
    const rest = new FakeRest(inOrder({ status: 200, json: body(3), headers: { ETag: '"v3"' } }));
    install(rest.fetch, rest.sleep);
    const first = await getPrompt(NAME);
    const second = await getPrompt(NAME);
    expect(first.version).toBe(3);
    expect(second).toBe(first);
    expect(rest.requests).toHaveLength(1);
    expect(rest.requests[0]?.url).toBe(`${BASE}/prompts/${NAME}/resolve?label=production`);
    expect(rest.requests[0]?.method).toBe("GET");
    expect(rest.requests[0]?.headers.Authorization).toBe("Bearer sk-cetra-test");
    expect(rest.requests[0]?.headers["If-None-Match"]).toBeUndefined();
  });

  it("never expires a version entry, whatever the TTL", async () => {
    const rest = new FakeRest(inOrder({ status: 200, json: body(2) }));
    install(rest.fetch, rest.sleep);
    await getPrompt(NAME, { version: 2 });
    clock += 3_600_000;
    await getPrompt(NAME, { version: 2 });
    await getPrompt(NAME, { version: 2, cacheTtlS: 0 });
    expect(rest.requests).toHaveLength(1);
    expect(rest.requests[0]?.url).toBe(`${BASE}/prompts/${NAME}/resolve?version=2`);
  });

  it("serves a stale label at once and revalidates it in the background", async () => {
    const rest = new FakeRest(
      inOrder(
        { status: 200, json: body(3), headers: { ETag: '"v3"' } },
        { status: 304, headers: { ETag: '"v3"' } },
        { status: 200, json: body(4), headers: { ETag: '"v4"' } },
      ),
    );
    const { cache } = install(rest.fetch, rest.sleep);
    expect((await getPrompt(NAME)).version).toBe(3);

    clock += 61_000;
    expect((await getPrompt(NAME)).version).toBe(3);
    await cache.settle();
    expect(rest.requests[1]?.headers["If-None-Match"]).toBe('"v3"');

    // The 304 reset the age: the label is fresh again.
    clock += 30_000;
    expect((await getPrompt(NAME)).version).toBe(3);
    expect(rest.requests).toHaveLength(2);

    clock += 61_000;
    expect((await getPrompt(NAME)).version).toBe(3);
    await cache.settle();
    expect(rest.requests[2]?.headers["If-None-Match"]).toBe('"v3"');
    expect((await getPrompt(NAME)).version).toBe(4);
  });

  it("starts one refresh per key", async () => {
    let fetches = 0;
    const fetch: RestFetch = () => {
      fetches += 1;
      if (fetches === 1) {
        return Promise.resolve(respond(200, body(3)));
      }
      return new Promise<Response>(() => {});
    };
    install(fetch);
    expect((await getPrompt(NAME)).version).toBe(3);
    clock += 61_000;
    expect((await getPrompt(NAME)).version).toBe(3);
    expect((await getPrompt(NAME)).version).toBe(3);
    expect(fetches).toBe(2);
  });

  it("fetches every time when cacheTtlS is 0", async () => {
    const rest = new FakeRest(
      inOrder({ status: 200, json: body(3) }, { status: 200, json: body(3) }),
    );
    install(rest.fetch, rest.sleep);
    await getPrompt(NAME, { cacheTtlS: 0 });
    await getPrompt(NAME, { cacheTtlS: 0 });
    expect(rest.requests).toHaveLength(2);
  });

  it("evicts the least recently used entry beyond 1 000", async () => {
    let fetches = 0;
    const fetch: RestFetch = async (url) => {
      fetches += 1;
      const version = Number(/version=(\d+)/.exec(url)?.[1]);
      return respond(200, body(version));
    };
    install(fetch);
    for (let version = 1; version <= 1_001; version += 1) {
      await getPrompt(NAME, { version });
    }
    expect(fetches).toBe(1_001);
    await getPrompt(NAME, { version: 1_001 });
    expect(fetches).toBe(1_001);
    await getPrompt(NAME, { version: 1 });
    expect(fetches).toBe(1_002);
  });
});

describe("getPrompt failures", () => {
  it("retries a network error and a 5xx, then succeeds", async () => {
    const rest = new FakeRest(
      inOrder(new Error("socket reset"), { status: 503 }, { status: 200, json: body(3) }),
    );
    install(rest.fetch, rest.sleep);
    expect((await getPrompt(NAME)).version).toBe(3);
    expect(rest.requests).toHaveLength(3);
    expect(rest.sleeps).toEqual([500, 1_000]);
  });

  it("caps Retry-After at 5 seconds", async () => {
    const rest = new FakeRest(
      inOrder({ status: 429, headers: { "Retry-After": "30" } }, { status: 200, json: body(3) }),
    );
    install(rest.fetch, rest.sleep);
    await getPrompt(NAME);
    expect(rest.sleeps).toEqual([5_000]);
  });

  it("does not retry a 404: the fallback answers, or PromptNotAvailable", async () => {
    const rest = new FakeRest(inOrder({ status: 404, json: { detail: "no such prompt" } }));
    install(rest.fetch, rest.sleep);
    const fallback = await getPrompt(NAME, { fallback: "Hi {{ name }}" });
    expect(fallback.isFallback).toBe(true);
    expect(fallback.version).toBeUndefined();
    expect(fallback.compile({ name: "Ada" })).toBe("Hi Ada");
    expect(rest.requests).toHaveLength(1);

    const rest2 = new FakeRest(inOrder({ status: 404 }));
    install(rest2.fetch, rest2.sleep);
    const error = await rejectionOf(getPrompt(NAME));
    expect(error).toBeInstanceOf(PromptNotAvailable);
    expect((error as PromptNotAvailable).reason).toBe("not found");
    expect((error as PromptNotAvailable).prompt).toBe(NAME);
    expect(rest2.requests).toHaveLength(1);
  });

  it("warns once a minute when a refresh fails, and keeps the stale prompt", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const rest = new FakeRest(
      inOrder(
        { status: 200, json: body(3, "TOP SECRET {{ name }}") },
        ...Array.from({ length: 9 }, () => ({ status: 500 })),
      ),
    );
    const { cache } = install(rest.fetch, rest.sleep);
    await getPrompt(NAME);

    clock += 61_000;
    expect((await getPrompt(NAME)).version).toBe(3);
    await cache.settle();
    clock += 1_000;
    expect((await getPrompt(NAME)).version).toBe(3);
    await cache.settle();
    expect(warn).toHaveBeenCalledTimes(1);
    const message = String(warn.mock.calls[0]?.[0]);
    expect(message).toContain(NAME);
    expect(message).toContain("status 500");
    expect(message).not.toContain("TOP SECRET");
    expect(message).not.toContain("sk-cetra-test");

    clock += 61_000;
    expect((await getPrompt(NAME)).version).toBe(3);
    await cache.settle();
    expect(warn).toHaveBeenCalledTimes(2);
  });

  it("treats a 200 that is not a prompt as a failure, without a retry", async () => {
    const rest = new FakeRest(inOrder({ status: 200, text: "not json" }));
    install(rest.fetch, rest.sleep);
    const error = await rejectionOf(getPrompt(NAME));
    expect((error as PromptNotAvailable).reason).toBe("invalid response");
    expect(rest.requests).toHaveLength(1);
  });

  it("is a failed fetch when the SDK is disabled", async () => {
    const fallback = await getPrompt(NAME, { fallback: [{ role: "user", content: "hi" }] });
    expect(fallback.type).toBe("chat");
    const error = await rejectionOf(getPrompt(NAME));
    expect(error).toBeInstanceOf(PromptNotAvailable);
    expect((error as PromptNotAvailable).reason).toBe("sdk disabled");
  });

  it("rejects programming errors with TypeError", async () => {
    await expect(getPrompt("")).rejects.toThrow(TypeError);
    await expect(getPrompt(NAME, { label: "a", version: 1 })).rejects.toThrow(TypeError);
    await expect(getPrompt(NAME, { version: 0 })).rejects.toThrow(TypeError);
    await expect(getPrompt(NAME, { version: 1.5 })).rejects.toThrow(TypeError);
    await expect(getPrompt(NAME, { cacheTtlS: -1 })).rejects.toThrow(TypeError);
  });
});

describe("Prompt.compile", () => {
  it("links the active span with the prompt name and version", async () => {
    const rest = new FakeRest(inOrder({ status: 200, json: CHAT_BODY }, { status: 404 }));
    const { exporter, runtime } = install(rest.fetch, rest.sleep);
    const prompt = await getPrompt(NAME);
    const missing = await getPrompt("missing", { fallback: "Hi {{ a }}" });

    runtime.tracer.startActiveSpan("ask", (span) => {
      const messages = prompt.compile({
        product: "cetra",
        question: "hi",
        history: [{ role: "user", content: "earlier" }],
      });
      expect(messages).toEqual([
        { role: "system", content: "You help cetra." },
        { role: "user", content: "earlier" },
        { role: "user", content: "hi" },
      ]);
      span.end();
    });
    runtime.tracer.startActiveSpan("fallback", (span) => {
      expect(missing.compile({ a: "1" })).toBe("Hi 1");
      span.end();
    });

    const [ask, fallback] = await finished(exporter);
    expect(ask?.attributes["cetra.prompt.name"]).toBe(NAME);
    expect(ask?.attributes["cetra.prompt.version"]).toBe(3);
    expect(fallback?.attributes["cetra.prompt.name"]).toBe("missing");
    expect(fallback?.attributes["cetra.prompt.version"]).toBeUndefined();
  });

  it("converts values as the contract says, and rejects other types", async () => {
    const prompt = await getPrompt("p", { fallback: "{{ flag }} {{ n }} {{ x }} {{ id }}" });
    expect(prompt.compile({ flag: true, n: 1.0, x: 2.5, id: 1 })).toBe("true 1 2.5 1");
    expect(prompt.compile({ flag: false, n: 0, x: 0, id: 0 })).toBe("false 0 0 0");
    expect(
      thrownBy(() => prompt.compile({ flag: true, n: { a: 1 } as unknown as string, x: 1, id: 1 })),
    ).toBeInstanceOf(TypeError);
  });

  it("raises PromptCompileError with the prompt name for a missing variable", async () => {
    const prompt = await getPrompt("p", { fallback: "{{ flag }} {{ n }}" });
    const error = thrownBy(() => prompt.compile({ flag: true }));
    expect(error).toBeInstanceOf(PromptCompileError);
    expect((error as PromptCompileError).kind).toBe("missing_variable");
    expect((error as PromptCompileError).message).toBe("prompt p: missing variable n");
  });

  it("raises PromptCompileError for a missing placeholder", async () => {
    const rest = new FakeRest(inOrder({ status: 200, json: CHAT_BODY }));
    install(rest.fetch, rest.sleep);
    const prompt = await getPrompt(NAME);
    const error = thrownBy(() => prompt.compile({ product: "cetra", question: "hi" }));
    expect((error as PromptCompileError).kind).toBe("missing_placeholder");
    expect((error as PromptCompileError).message).toBe(
      `prompt ${NAME}: missing placeholder history`,
    );
  });

  it("rejects a body that is not a prompt", () => {
    expect(parsePrompt({ name: NAME, type: "text", version: 1, template: 5 })).toBeUndefined();
    expect(
      parsePrompt({
        name: NAME,
        type: "text",
        version: 1,
        template: "{% x %}",
        config: {},
        labels: [],
      }),
    ).toBeUndefined();
    expect(parsePrompt(null)).toBeUndefined();
  });
});

describe("logs", () => {
  it("never holds the template or the key", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const rest = new FakeRest(
      inOrder(
        { status: 200, json: body(3, "TOP SECRET {{ name }}") },
        ...Array.from({ length: 3 }, () => ({ status: 500 })),
      ),
    );
    const { cache } = install(rest.fetch, rest.sleep);
    await getPrompt(NAME);
    clock += 61_000;
    await getPrompt(NAME);
    await cache.settle();
    const logged = warn.mock.calls.flat().join("\n");
    expect(logged).toContain(NAME);
    expect(logged).not.toContain("TOP SECRET");
    expect(logged).not.toContain("sk-cetra-test");
  });
});

describe("client.prompts", () => {
  it("creates, versions, labels, reads and lists prompts", async () => {
    const rest = new FakeRest(
      inOrder(
        { status: 201, json: body(1) },
        { status: 201, json: body(2) },
        { status: 204 },
        { status: 200, json: body(2) },
        { status: 200, json: { data: [{ name: "a" }, { name: "b" }], next_cursor: "c2" } },
        { status: 200, json: { data: [{ name: "c" }], next_cursor: null } },
      ),
    );
    const client = new Client({
      apiKey: "sk-cetra-test",
      baseUrl: "http://cetra.test",
      fetch: rest.fetch,
      sleep: rest.sleep,
    });

    const created = await client.prompts.create({
      name: NAME,
      type: "text",
      template: "Hello {{ name }}",
      labels: ["production"],
      commitMessage: "first",
      tags: ["support"],
    });
    expect(created).toBeInstanceOf(Prompt);
    expect(rest.requests[0]).toMatchObject({ method: "POST", url: `${BASE}/prompts` });
    expect(JSON.parse(rest.requests[0]?.body ?? "{}")).toEqual({
      name: NAME,
      type: "text",
      template: "Hello {{ name }}",
      labels: ["production"],
      commit_message: "first",
      tags: ["support"],
    });

    await client.prompts.createVersion(NAME, { template: "Hi {{ name }}" });
    expect(rest.requests[1]).toMatchObject({
      method: "POST",
      url: `${BASE}/prompts/${NAME}/versions`,
    });
    expect(JSON.parse(rest.requests[1]?.body ?? "{}")).toEqual({ template: "Hi {{ name }}" });

    await client.prompts.setLabel(NAME, "production", 2);
    expect(rest.requests[2]).toMatchObject({
      method: "PUT",
      url: `${BASE}/prompts/${NAME}/labels/production`,
    });
    expect(JSON.parse(rest.requests[2]?.body ?? "{}")).toEqual({ version: 2 });

    const read = await client.prompts.get(NAME, { label: "production" });
    expect(read.version).toBe(2);
    expect(rest.requests[3]?.url).toBe(`${BASE}/prompts/${NAME}/resolve?label=production`);

    expect(await client.prompts.list()).toEqual(["a", "b", "c"]);
    expect(rest.requests[4]?.url).toBe(`${BASE}/prompts?limit=1000`);
    expect(rest.requests[5]?.url).toBe(`${BASE}/prompts?limit=1000&cursor=c2`);
  });

  it("raises ApiError when an answer is not a prompt", async () => {
    const rest = new FakeRest(inOrder({ status: 200, json: {} }));
    const client = new Client({
      apiKey: "sk-cetra-test",
      baseUrl: "http://cetra.test",
      fetch: rest.fetch,
      sleep: rest.sleep,
    });
    const error = await rejectionOf(client.prompts.get(NAME));
    expect(error).toBeInstanceOf(ApiError);
    expect((error as ApiError).detail).toBe("invalid response");
    expect((error as ApiError).code).toBe("invalid_response");
  });
});
