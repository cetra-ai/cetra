import { beforeEach, expect, it } from "vitest";
import { server } from "@/mocks/node";
import { projectHandlers, projectOfRef, storyProject } from "@/mocks/projects";

beforeEach(() => {
  server.use(...projectHandlers);
});

async function get(path: string) {
  const response = await fetch(new URL(path, window.location.origin));
  return { status: response.status, body: (await response.json()) as Record<string, unknown> };
}

type Row = { id: string; slug: string; name: string };
type Activity = {
  project_id: string;
  traces: number;
  errors: number;
  cost_usd: number;
  buckets: { traces: number; errors: number }[];
};

it("the project list adds the story projects", async () => {
  const list = await get("/api/v1/orgs/acme/projects");
  const rows = list.body.data as Row[];
  expect(rows.map((row) => [row.slug, row.name])).toEqual([
    ["support-bot", "Support bot"],
    ["playground", "Playground"],
    ["code-agent", "Code agent"],
    ["voice-desk", "Voice desk"],
    ["invoice-pipeline", "Invoice pipeline"],
  ]);
  expect(rows.slice(2).map((row) => row.id)).toEqual([
    "01900000-0000-7000-8000-000000000003",
    "01900000-0000-7000-8000-000000000004",
    "01900000-0000-7000-8000-000000000005",
  ]);
});

it("a story project resolves by ref", async () => {
  const project = await get("/api/v1/projects/acme%2Fvoice-desk");
  expect(project.status).toBe(200);
  expect(project.body).toMatchObject({
    id: "01900000-0000-7000-8000-000000000004",
    slug: "voice-desk",
    name: "Voice desk",
  });
});

it("other projects keep the fixture", async () => {
  const project = await get("/api/v1/projects/acme%2Fsupport-bot");
  expect(project.body).toMatchObject({ slug: "support-bot" });
});

it("activity covers the story projects", async () => {
  const activity = await get("/api/v1/orgs/acme/projects/activity");
  const entries = activity.body.data as Activity[];
  expect(entries).toHaveLength(5);
  const ids = entries.map((entry) => entry.project_id);
  expect(ids).toEqual(
    expect.arrayContaining([
      "01900000-0000-7000-8000-000000000003",
      "01900000-0000-7000-8000-000000000004",
      "01900000-0000-7000-8000-000000000005",
    ]),
  );
  for (const entry of entries.slice(2)) {
    const traces = entry.buckets.reduce((sum, bucket) => sum + bucket.traces, 0);
    const errors = entry.buckets.reduce((sum, bucket) => sum + bucket.errors, 0);
    expect(traces).toBe(entry.traces);
    expect(errors).toBe(entry.errors);
  }
});

it("storyProject and projectOfRef map the stories to their projects", () => {
  expect(storyProject("voice").slug).toBe("voice-desk");
  expect(projectOfRef("acme/invoice-pipeline")?.story).toBe("extraction");
  expect(projectOfRef("acme/playground")).toBeUndefined();
  expect(projectOfRef(undefined)).toBeUndefined();
});
