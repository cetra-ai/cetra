import { beforeEach, expect, it } from "vitest";
import { server } from "@/mocks/node";
import { storyPromptHandlers } from "@/mocks/stories-prompts";

const PROJECT = "/api/v1/projects/acme%2Fsupport-bot";

beforeEach(() => {
  server.use(...storyPromptHandlers);
});

async function get(path: string) {
  const response = await fetch(new URL(`${PROJECT}${path}`, window.location.origin));
  return { status: response.status, body: (await response.json()) as Record<string, unknown> };
}

type Named = { name: string };
type Version = {
  version: number;
  labels: string[];
  variables?: string[];
  config?: { model: string; params: { max_tokens: number } };
  template?: unknown;
  commit_message?: string;
};

it("lists the fixture prompts, then the three story prompts", async () => {
  const list = await get("/prompts");
  expect((list.body.data as Named[]).map((prompt) => prompt.name)).toEqual([
    "summarize",
    "support-reply",
    "write-patch",
    "voice-reply",
    "extract-invoice",
  ]);
  expect(list.body.next_cursor).toBeNull();
});

it("filters the list by q, label and tag", async () => {
  const names = async (query: string) =>
    ((await get(`/prompts${query}`)).body.data as Named[]).map((prompt) => prompt.name);

  expect(await names("?q=INVOICE")).toEqual(["extract-invoice"]);
  expect(await names("?tag=voice")).toEqual(["voice-reply"]);
  expect(await names("?label=production")).toEqual([
    "summarize",
    "support-reply",
    "write-patch",
    "voice-reply",
    "extract-invoice",
  ]);
  expect(await names("?q=zzz")).toEqual([]);
});

it("gives each story prompt its shown version as the latest, labelled latest and production", async () => {
  const expectations: [string, number][] = [
    ["write-patch", 3],
    ["voice-reply", 4],
    ["extract-invoice", 5],
  ];
  for (const [name, latest] of expectations) {
    const prompt = await get(`/prompts/${name}`);
    expect(prompt.body).toMatchObject({ name, type: "chat", latest_version: latest });

    const versions = (await get(`/prompts/${name}/versions`)).body.data as Version[];
    expect(versions.map((version) => version.version)).toEqual(
      Array.from({ length: latest }, (_, index) => latest - index),
    );
    expect(versions[0]?.labels).toEqual(["latest", "production"]);
    for (const version of versions.slice(1)) {
      expect(version.labels).toEqual([]);
    }
  }
});

it("serves the shown version's template, variables and model", async () => {
  const patch = await get("/prompts/write-patch/versions/3");
  expect(patch.body).toMatchObject({
    variables: ["issue", "path", "code"],
    config: { model: "claude-sonnet-4-5" },
    commit_message: "Name the file and ask for the diff only",
    template: [
      { role: "system" },
      { role: "user", content: "Issue: {{ issue }}\n\nFile {{ path }}:\n{{ code }}" },
    ],
  });

  const voice = await get("/prompts/voice-reply/versions/4");
  expect(voice.body).toMatchObject({
    variables: ["caller", "slots"],
    config: { model: "gpt-4.1-mini" },
  });

  const invoice = await get("/prompts/extract-invoice/versions/5");
  expect(invoice.body).toMatchObject({
    variables: ["ocr_text"],
    config: { model: "gemini-2.5-flash", params: { max_tokens: 600 } },
  });
});

it("resolves production and latest to the latest version, and leaves other labels to the manifest", async () => {
  expect((await get("/prompts/voice-reply/resolve")).body).toMatchObject({ version: 4 });
  expect((await get("/prompts/voice-reply/resolve?label=latest")).body).toMatchObject({
    version: 4,
  });
  expect((await get("/prompts/voice-reply/resolve?label=staging")).status).toBe(404);
});

it("records one label event per version, newest first", async () => {
  const events = (await get("/prompts/extract-invoice/label-events")).body.data as {
    to_version: number;
    from_version?: number;
  }[];
  expect(events).toHaveLength(5);
  expect(events.map((event) => event.to_version)).toEqual([5, 4, 3, 2, 1]);
  expect(events[events.length - 1]).not.toHaveProperty("from_version");
  expect(events[0]).toMatchObject({ from_version: 4 });
});

it("answers prompt metrics for any window, newest version first", async () => {
  const from = "2026-11-13T00:00:00.000Z";
  const to = "2026-11-20T00:00:00.000Z";
  const metrics = await get(`/prompts/write-patch/metrics?from=${from}&to=${to}`);
  expect(metrics.body).toMatchObject({ from, to });
  expect((metrics.body.versions as { version: number }[]).map((item) => item.version)).toEqual([
    3, 2, 1,
  ]);
});

it("leaves an unknown prompt, a version out of range and the fixture prompts to other handlers", async () => {
  expect((await get("/prompts/nope")).status).toBe(404);
  expect((await get("/prompts/write-patch/versions/4")).status).toBe(404);
  expect((await get("/prompts/write-patch/versions/0")).status).toBe(404);
  expect(await get("/prompts/support-reply")).toMatchObject({
    body: { name: "support-reply" },
  });
});

it("gives every story prompt an id of its own, with versions dated before the story clock", async () => {
  const ids = await Promise.all(
    ["write-patch", "voice-reply", "extract-invoice"].map(
      async (name) => (await get(`/prompts/${name}`)).body.id,
    ),
  );
  expect(ids).toEqual([
    "01900000-0000-7000-8000-000000570702",
    "01900000-0000-7000-8000-000000570703",
    "01900000-0000-7000-8000-000000570704",
  ]);

  const versions = (await get("/prompts/extract-invoice/versions")).body.data as {
    created_at: string;
  }[];
  const dates = versions.map((version) => version.created_at).reverse();
  for (const date of dates) {
    expect(date < "2026-11-19T12:00:00Z").toBe(true);
  }
  expect([...dates].sort()).toEqual(dates);
});
