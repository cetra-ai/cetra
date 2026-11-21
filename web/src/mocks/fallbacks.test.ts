import { beforeEach, expect, it } from "vitest";
import { fallbackHandlers } from "@/mocks/fallbacks";
import { server } from "@/mocks/node";

const PROJECT = "/api/v1/projects/acme%2Fsupport-bot";

beforeEach(() => {
  server.use(...fallbackHandlers);
});

async function get(path: string) {
  const response = await fetch(new URL(`${PROJECT}${path}`, window.location.origin));
  return { status: response.status, body: (await response.json()) as Record<string, unknown> };
}

it("a prompt without fixtures has its detail, versions and labels", async () => {
  const prompt = await get("/prompts/summarize");
  expect(prompt).toMatchObject({ status: 200, body: { name: "summarize", type: "text" } });

  const versions = await get("/prompts/summarize/versions");
  expect(versions.body.data).toHaveLength(1);
  const version = await get("/prompts/summarize/versions/1");
  expect(version.body).toMatchObject({ version: 1, labels: ["latest", "production"] });
  expect(typeof version.body.template).toBe("string");

  expect((await get("/prompts/summarize/versions/2")).status).toBe(404);
  expect((await get("/prompts/summarize/label-events")).body.data).toEqual([
    { label: "production", to_version: 1, at: "2026-01-01T00:00:00.000000000Z" },
  ]);
});

it("prompt metrics answer any window, newest version first", async () => {
  const from = "2026-11-13T00:00:00.000Z";
  const to = "2026-11-20T00:00:00.000Z";
  const metrics = await get(`/prompts/support-reply/metrics?from=${from}&to=${to}`);
  expect(metrics.body).toMatchObject({ from, to });
  expect((metrics.body.versions as { version: number }[]).map((item) => item.version)).toEqual([
    2, 1,
  ]);
});

it("the fixture prompt and an unknown prompt fall through", async () => {
  expect((await get("/prompts/support-reply")).body.name).toBe("support-reply");
  expect((await get("/prompts/nope")).status).toBe(404);
});

it("every listed issue source opens, an unknown one is not found", async () => {
  const topics = await get("/issue-sources/01900000-0000-7000-8000-000000001c20");
  expect(topics.body).toMatchObject({ name: "Topics", kind: "topic" });
  expect((await get("/issue-sources/01900000-0000-7000-8000-000000001c00")).status).toBe(200);
  expect((await get("/issue-sources/01900000-0000-7000-8000-00000000ffff")).status).toBe(404);
});
