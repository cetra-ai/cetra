import assert from "node:assert/strict";
import { test } from "node:test";
import { fetchStars, formatStars } from "./github-stars.mjs";

/** A fake fetch that returns the given response (or throws when given an Error). */
function fakeFetch(response, calls = []) {
  return async (url, init) => {
    calls.push({ url, init });
    if (response instanceof Error) throw response;
    return response;
  };
}

test("returns the count", async () => {
  const n = await fetchStars("a/b", {
    fetchImpl: fakeFetch({ ok: true, json: async () => ({ stargazers_count: 1234 }) }),
  });
  assert.equal(n, 1234);
});

test("null on a non-ok response", async () => {
  const n = await fetchStars("a/b", { fetchImpl: fakeFetch({ ok: false }) });
  assert.equal(n, null);
});

test("null when the field is missing or not an integer", async () => {
  for (const body of [{}, { stargazers_count: "12" }, { stargazers_count: -1 }]) {
    const n = await fetchStars("a/b", {
      fetchImpl: fakeFetch({ ok: true, json: async () => body }),
    });
    assert.equal(n, null, JSON.stringify(body));
  }
});

test("null when fetch throws", async () => {
  const n = await fetchStars("a/b", { fetchImpl: fakeFetch(new Error("offline")) });
  assert.equal(n, null);
});

test("sends the user agent and calls the right URL", async () => {
  const calls = [];
  await fetchStars("a/b", {
    fetchImpl: fakeFetch({ ok: false }, calls),
  });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, "https://api.github.com/repos/a/b");
  assert.equal(calls[0].init.headers["user-agent"], "cetra-website-build");
});

test("sends the token when GITHUB_TOKEN is set", async () => {
  const saved = process.env.GITHUB_TOKEN;
  process.env.GITHUB_TOKEN = "test-token";
  try {
    const calls = [];
    await fetchStars("a/b", { fetchImpl: fakeFetch({ ok: false }, calls) });
    assert.equal(calls[0].init.headers.authorization, "Bearer test-token");
  } finally {
    if (saved === undefined) delete process.env.GITHUB_TOKEN;
    else process.env.GITHUB_TOKEN = saved;
  }
});

test("formats counts", () => {
  const cases = [
    [0, "0"],
    [999, "999"],
    [1000, "1k"],
    [1250, "1.3k"],
    [12345, "12.3k"],
    [999949, "999.9k"],
    [999950, "1M"],
    [1000000, "1M"],
    [2500000, "2.5M"],
  ];
  for (const [n, want] of cases) {
    assert.equal(formatStars(n), want, `formatStars(${n})`);
  }
});
