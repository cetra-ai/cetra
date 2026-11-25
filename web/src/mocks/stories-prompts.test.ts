import { beforeEach, expect, it } from "vitest";
import { fallbackHandlers } from "@/mocks/fallbacks";
import { server } from "@/mocks/node";
import { type StoryId, storyProject } from "@/mocks/projects";
import { storyPromptHandlers } from "@/mocks/stories-prompts";

/** The story whose project serves each story prompt (P-4). */
const STORY_OF: Record<string, StoryId> = {
  "write-patch": "coding",
  "voice-reply": "voice",
  "extract-invoice": "extraction",
  "support-reply": "support",
};

beforeEach(() => {
  server.use(...storyPromptHandlers);
});

/** A GET of `path` under the project with API ref `ref` (`acme/<slug>`). */
async function getIn(ref: string, path: string) {
  const url = new URL(`/api/v1/projects/${encodeURIComponent(ref)}${path}`, window.location.origin);
  const response = await fetch(url);
  return { status: response.status, body: (await response.json()) as Record<string, unknown> };
}

/** A GET of `path` under the project that serves the story's prompt. */
async function get(story: StoryId, path: string) {
  return getIn(storyProject(story).ref, path);
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

it("lists the fixture prompts, then the story prompt of each project", async () => {
  const namesIn = async (ref: string) =>
    ((await getIn(ref, "/prompts")).body.data as Named[]).map((prompt) => prompt.name);

  expect(await namesIn("acme/code-agent")).toEqual(["summarize", "support-reply", "write-patch"]);
  expect(await namesIn("acme/voice-desk")).toEqual(["summarize", "support-reply", "voice-reply"]);
  expect(await namesIn("acme/invoice-pipeline")).toEqual([
    "summarize",
    "support-reply",
    "extract-invoice",
  ]);
  expect(await namesIn("acme/support-bot")).toEqual(["summarize", "support-reply"]);

  const list = await getIn("acme/code-agent", "/prompts");
  expect(list.body.next_cursor).toBeNull();
});

it("filters the list by q, label and tag", async () => {
  const names = async (ref: string, query: string) =>
    ((await getIn(ref, `/prompts${query}`)).body.data as Named[]).map((prompt) => prompt.name);

  expect(await names("acme/invoice-pipeline", "?q=INVOICE")).toEqual(["extract-invoice"]);
  expect(await names("acme/voice-desk", "?tag=voice")).toEqual(["voice-reply"]);
  expect(await names("acme/voice-desk", "?label=production")).toEqual([
    "summarize",
    "support-reply",
    "voice-reply",
  ]);
  expect(await names("acme/voice-desk", "?q=zzz")).toEqual([]);
});

it("gives each story prompt its shown version as the latest, labelled latest and production", async () => {
  const expectations: [string, number][] = [
    ["write-patch", 4],
    ["voice-reply", 5],
    ["extract-invoice", 6],
  ];
  for (const [name, latest] of expectations) {
    const story = STORY_OF[name] ?? "support";
    const prompt = await get(story, `/prompts/${name}`);
    expect(prompt.body).toMatchObject({ name, type: "chat", latest_version: latest });

    const versions = (await get(story, `/prompts/${name}/versions`)).body.data as Version[];
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
  const patch = await get("coding", "/prompts/write-patch/versions/3");
  expect(patch.body).toMatchObject({
    variables: ["issue", "path", "code"],
    config: { model: "claude-sonnet-4-5" },
    commit_message: "Name the file and ask for the diff only",
    template: [
      { role: "system" },
      { role: "user", content: "Issue: {{ issue }}\n\nFile {{ path }}:\n{{ code }}" },
    ],
  });

  const voice = await get("voice", "/prompts/voice-reply/versions/4");
  expect(voice.body).toMatchObject({
    variables: ["caller", "slots"],
    config: { model: "gpt-4.1-mini" },
  });

  const invoice = await get("extraction", "/prompts/extract-invoice/versions/5");
  expect(invoice.body).toMatchObject({
    variables: ["ocr_text"],
    config: { model: "gemini-2.5-flash", params: { max_tokens: 600 } },
  });
});

it("resolves production and latest to the latest version, and leaves other labels to the manifest", async () => {
  expect((await get("voice", "/prompts/voice-reply/resolve")).body).toMatchObject({ version: 5 });
  expect((await get("voice", "/prompts/voice-reply/resolve?label=latest")).body).toMatchObject({
    version: 5,
  });
  expect((await get("voice", "/prompts/voice-reply/resolve?label=staging")).status).toBe(404);
});

it("resolves a label to the version that carries it, in the support project", async () => {
  expect((await get("support", "/prompts/support-reply/resolve")).body).toMatchObject({
    version: 3,
    labels: ["latest", "production"],
  });
  expect((await get("support", "/prompts/support-reply/resolve?label=staging")).body).toMatchObject(
    {
      version: 2,
      labels: ["staging"],
    },
  );
});

it("records one label event per version, newest first", async () => {
  const events = (await get("extraction", "/prompts/extract-invoice/label-events")).body.data as {
    to_version: number;
    from_version?: number;
  }[];
  expect(events).toHaveLength(6);
  expect(events.map((event) => event.to_version)).toEqual([6, 5, 4, 3, 2, 1]);
  expect(events[events.length - 1]).not.toHaveProperty("from_version");
  expect(events[0]).toMatchObject({ from_version: 5 });
});

it("answers prompt metrics for any window, newest version first", async () => {
  const from = "2026-11-13T00:00:00.000Z";
  const to = "2026-11-20T00:00:00.000Z";
  const metrics = await get("coding", `/prompts/write-patch/metrics?from=${from}&to=${to}`);
  expect(metrics.body).toMatchObject({ from, to });
  expect((metrics.body.versions as { version: number }[]).map((item) => item.version)).toEqual([
    4, 3, 2, 1,
  ]);
});

it("leaves an unknown prompt, a version out of range and the fixture prompts to other handlers", async () => {
  expect((await get("coding", "/prompts/nope")).status).toBe(404);
  expect((await get("coding", "/prompts/write-patch/versions/5")).status).toBe(404);
  expect((await get("coding", "/prompts/write-patch/versions/0")).status).toBe(404);
  expect(await get("support", "/prompts/support-reply")).toMatchObject({
    body: { name: "support-reply" },
  });
});

it("gives every story prompt an id of its own, with versions dated before the story clock", async () => {
  const ids = await Promise.all(
    ["write-patch", "voice-reply", "extract-invoice"].map(
      async (name) => (await get(STORY_OF[name] ?? "support", `/prompts/${name}`)).body.id,
    ),
  );
  expect(ids).toEqual([
    "01900000-0000-7000-8000-000000570702",
    "01900000-0000-7000-8000-000000570703",
    "01900000-0000-7000-8000-000000570704",
  ]);

  const versions = (await get("extraction", "/prompts/extract-invoice/versions")).body.data as {
    created_at: string;
  }[];
  const dates = versions.map((version) => version.created_at).reverse();
  for (const date of dates) {
    expect(date < "2026-11-19T12:00:00Z").toBe(true);
  }
  expect([...dates].sort()).toEqual(dates);
});

it("a story prompt is not served in another project", async () => {
  expect((await getIn("acme/code-agent", "/prompts/voice-reply")).status).toBe(404);
  expect((await getIn("acme/voice-desk", "/prompts/voice-reply")).status).toBe(200);
});

it("adds one version to each story prompt, its line last in the system message", async () => {
  const added: [string, string, number, string, string][] = [
    [
      "write-patch",
      "coding",
      4,
      "Split changes over 20 files into steps.",
      "Split large refactors into steps",
    ],
    [
      "voice-reply",
      "voice",
      5,
      "Read the booking back in one sentence: day, time and doctor.",
      "Read the booking back",
    ],
    [
      "extract-invoice",
      "extraction",
      6,
      "Amounts may use comma decimals (1.234,56 means 1234.56).",
      "Accept comma decimals",
    ],
  ];
  for (const [name, story, version, line, commit] of added) {
    const detail = await get(story as StoryId, `/prompts/${name}/versions/${version}`);
    const previous = await get(story as StoryId, `/prompts/${name}/versions/${version - 1}`);
    const system = (detail.body.template as { role: string; content: string }[])[0]?.content ?? "";
    const before =
      (previous.body.template as { role: string; content: string }[])[0]?.content ?? "";
    expect(system.endsWith(line)).toBe(true);
    expect(system).toBe(`${before} ${line}`);
    expect(detail.body).toMatchObject({
      version,
      commit_message: commit,
      labels: ["latest", "production"],
    });
    expect(detail.body.config).toEqual(previous.body.config);
    expect(detail.body.variables).toEqual(previous.body.variables);
    expect(previous.body.labels).toEqual([]);
  }
});

it("serves support-reply in the support project as three versions, v3 labelled production and v2 staging", async () => {
  const prompt = await get("support", "/prompts/support-reply");
  expect(prompt.body).toMatchObject({
    id: "01900000-0000-7000-8000-000000001001",
    name: "support-reply",
    type: "chat",
    tags: [],
    latest_version: 3,
    created_at: "2026-01-01T00:00:01.000000000Z",
    updated_at: "2026-01-01T10:30:00Z",
    labels: [
      { label: "production", version: 3, updated_at: "2026-01-01T10:30:00Z" },
      { label: "staging", version: 2, updated_at: "2026-01-01T00:00:02.000000000Z" },
    ],
  });
  expect(prompt.body).not.toHaveProperty("description");

  const list = (await get("support", "/prompts")).body.data as {
    id: string;
    name: string;
    latest_version: number;
  }[];
  expect(list.filter((item) => item.name === "support-reply")).toEqual([
    expect.objectContaining({ id: "01900000-0000-7000-8000-000000001001", latest_version: 3 }),
  ]);

  const versions = (await get("support", "/prompts/support-reply/versions")).body.data as Version[];
  expect(versions.map((version) => version.version)).toEqual([3, 2, 1]);
  expect(versions.map((version) => version.labels)).toEqual([
    ["latest", "production"],
    ["staging"],
    [],
  ]);

  const v2 = await get("support", "/prompts/support-reply/versions/2");
  const v3 = await get("support", "/prompts/support-reply/versions/3");
  expect(v2.body.labels).toEqual(["staging"]);
  expect(v3.body).toMatchObject({
    variables: ["product", "language", "question"],
    placeholders: ["history"],
    config: { model: "gpt-4o", params: { temperature: 0.3, max_tokens: 512 } },
    commit_message: "Say plainly what cetra doesn't do",
    labels: ["latest", "production"],
  });
  expect(v2.body.placeholders).toEqual(["history"]);
  const systemOf = (body: Record<string, unknown>) =>
    (body.template as { role?: string; content?: string }[])[0]?.content;
  expect(systemOf(v3.body)).toBe(
    `${systemOf(v2.body)} If the customer asks for something cetra doesn't do, say so plainly, then offer the closest thing it does.`,
  );
});

it("records support-reply's label events newest first, and answers its metrics for all three versions", async () => {
  const events = (await get("support", "/prompts/support-reply/label-events")).body.data as {
    label: string;
    to_version: number;
    from_version?: number;
  }[];
  expect(events.map((event) => event.to_version)).toEqual([3, 2, 1]);
  expect(events[0]).toMatchObject({ label: "production", from_version: 1, to_version: 3 });
  expect(events[events.length - 1]).not.toHaveProperty("from_version");

  const from = "2026-01-01T00:00:00.000Z";
  const to = "2026-01-02T00:00:00.000Z";
  const metrics = await get("support", `/prompts/support-reply/metrics?from=${from}&to=${to}`);
  expect((metrics.body.versions as { version: number }[]).map((item) => item.version)).toEqual([
    3, 2, 1,
  ]);
});

it("keeps the fixture support-reply, versions 1 and 2, in the other projects", async () => {
  server.use(...fallbackHandlers);
  const versions = (await getIn("acme/code-agent", "/prompts/support-reply/versions")).body
    .data as Version[];
  expect(versions.map((version) => version.version)).toEqual([2, 1]);
  const list = (await getIn("acme/code-agent", "/prompts")).body.data as {
    name: string;
    latest_version: number;
  }[];
  expect(list.filter((prompt) => prompt.name === "support-reply")).toEqual([
    expect.objectContaining({ latest_version: 2 }),
  ]);
});
