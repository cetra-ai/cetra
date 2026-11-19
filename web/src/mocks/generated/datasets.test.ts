import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { createDataset } from "@/mocks/generated/dataset";
import { createDatasets, type GeneratedDataset, itemsAt } from "@/mocks/generated/datasets";
import { largeHandlers } from "@/mocks/generated/handlers";
import { server } from "@/mocks/node";

const NOW = Date.parse("2026-06-01T00:00:00.000Z");
const PROJECT = `${location.origin}/api/v1/projects/acme%2Fsupport-bot`;

const traces = createDataset({ seed: 1, now: NOW, traces: 50 }).traces;

function generate(): GeneratedDataset[] {
  return createDatasets({ seed: 1, now: NOW, traces });
}

function named(datasets: GeneratedDataset[], name: string): GeneratedDataset {
  const found = datasets.find((item) => item.dataset.name === name);
  if (!found) {
    throw new Error(`no dataset ${name}`);
  }
  return found;
}

function withCursor(base: string, cursor: string | null): string {
  if (cursor === null) {
    return base;
  }
  return `${base}${base.includes("?") ? "&" : "?"}cursor=${encodeURIComponent(cursor)}`;
}

/** Every row of a paginated list under `key`, following `next_cursor` until it is null. */
async function collect<T>(base: string, key: "data" | "rows" = "data"): Promise<T[]> {
  const rows: T[] = [];
  let cursor: string | null = null;
  for (;;) {
    const response = await fetch(withCursor(base, cursor));
    const body = (await response.json()) as Record<typeof key, T[]> & {
      next_cursor: string | null;
    };
    rows.push(...body[key]);
    cursor = body.next_cursor;
    if (cursor === null) {
      return rows;
    }
  }
}

it("generates the same datasets for the same inputs", () => {
  expect(generate()).toEqual(generate());
});

it("generates 24 datasets with unique names, the last one empty", () => {
  const datasets = generate();
  expect(datasets).toHaveLength(24);
  expect(new Set(datasets.map((item) => item.dataset.name)).size).toBe(24);

  const empty = named(datasets, "empty-drafts");
  expect(empty.dataset.current_version).toBe(0);
  expect(empty.dataset.item_count).toBe(0);
  expect(empty.items).toHaveLength(0);
  expect(empty.versions).toHaveLength(0);
  expect(empty.experiments).toHaveLength(0);
});

it("gives two datasets an input schema and a description to about two thirds", () => {
  const datasets = generate();
  expect(datasets.filter((item) => item.dataset.input_schema !== undefined)).toHaveLength(2);
  expect(datasets.filter((item) => item.dataset.description !== undefined)).toHaveLength(16);
});

it("keeps every version, item and experiment count consistent", () => {
  for (const generated of generate()) {
    const { dataset } = generated;
    expect(generated.versions.map((item) => item.version)).toEqual(
      Array.from({ length: dataset.current_version }, (_, index) => index + 1),
    );
    expect(generated.items).toHaveLength(dataset.item_count);
    expect(generated.experiments).toHaveLength(dataset.experiment_count);
    expect(Date.parse(dataset.updated_at)).toBeGreaterThanOrEqual(Date.parse(dataset.created_at));
    for (const item of generated.items) {
      expect(item.version).toBeGreaterThanOrEqual(1);
      expect(item.version).toBeLessThanOrEqual(dataset.current_version);
    }
    for (const { experiment, summary, rows } of generated.experiments) {
      expect(experiment.dataset_version).toBeLessThanOrEqual(dataset.current_version);
      expect(experiment.item_count).toBe(itemsAt(generated, experiment.dataset_version).length);
      expect(experiment.result_count).toBe(rows.length);
      expect(summary.result_count).toBe(rows.length);
      expect(summary.experiment).toEqual(experiment);
      if (experiment.status === "completed") {
        expect(experiment.result_count).toBe(experiment.item_count * experiment.repetitions);
        expect(experiment.finished_at).toBeDefined();
      }
    }
  }
});

it("points every item source at a generated trace, and at its root span", () => {
  const traceIds = new Set(traces.map((trace) => trace.trace_id));
  const rootOf = new Map(traces.map((trace) => [trace.trace_id, trace.root_span_id]));
  const sourced = generate().flatMap((item) => item.items.filter((row) => row.source_trace_id));
  expect(sourced.length).toBeGreaterThan(0);
  for (const row of sourced) {
    expect(traceIds.has(row.source_trace_id ?? "")).toBe(true);
    expect(row.source_span_id).toBe(rootOf.get(row.source_trace_id ?? ""));
  }
});

it("marks one experiment failed with an error and some running", () => {
  const experiments = generate().flatMap((item) => item.experiments.map((row) => row.experiment));
  expect(experiments.filter((row) => row.status === "failed")).toHaveLength(1);
  expect(experiments.find((row) => row.status === "failed")?.error).toBeDefined();
  expect(experiments.filter((row) => row.status === "running").length).toBeGreaterThan(0);
  for (const row of experiments.filter((item) => item.status === "running")) {
    expect(row.finished_at).toBeUndefined();
  }
});

it("lists every dataset, item, version and experiment exactly once over pages", async () => {
  server.use(...largeHandlers(createDataset({ seed: 1, now: NOW, traces: 50 })));
  const datasets = await collect<Schema<"Dataset">>(`${PROJECT}/datasets?limit=5`);
  expect(datasets.map((row) => row.name)).toHaveLength(24);
  expect(new Set(datasets.map((row) => row.id)).size).toBe(24);

  const faq = named(generate(), "faq");
  const items = await collect<Schema<"DatasetItem">>(`${PROJECT}/datasets/faq/items?limit=7`);
  expect(items.map((row) => row.id)).toEqual(faq.items.map((row) => row.id));

  const versions = await collect<Schema<"DatasetVersion">>(
    `${PROJECT}/datasets/faq/versions?limit=4`,
  );
  expect(versions.map((row) => row.version)).toEqual(
    faq.versions.map((row) => row.version).reverse(),
  );
});

it("filters datasets by name and answers 404 for an unknown dataset", async () => {
  server.use(...largeHandlers(createDataset({ seed: 1, now: NOW, traces: 50 })));
  const matches = await collect<Schema<"Dataset">>(`${PROJECT}/datasets?q=REFUND`);
  expect(matches.map((row) => row.name)).toEqual(["refund-requests"]);

  const missing = await fetch(`${PROJECT}/datasets/no-such-dataset`);
  expect(missing.status).toBe(404);
});

it("answers the results of an experiment with its summary and pages of rows", async () => {
  server.use(...largeHandlers(createDataset({ seed: 1, now: NOW, traces: 50 })));
  const faq = named(generate(), "faq");
  const target = faq.experiments.find((item) => item.experiment.status === "completed");
  if (!target) {
    throw new Error("faq has no completed experiment");
  }
  const id = target.experiment.id;
  const rows = await collect<Schema<"ResultRow">>(
    `${PROJECT}/experiments/${id}/results?limit=3`,
    "rows",
  );
  expect(rows).toHaveLength(target.rows.length);
  const first = await fetch(`${PROJECT}/experiments/${id}/results?limit=3`);
  const body = (await first.json()) as { summary: Schema<"ExperimentSummary">; rows: unknown[] };
  expect(body.summary).toEqual(target.summary);
  expect(body.rows).toHaveLength(3);
});

/** The JSON text the item search reads (D-J-22), computed here from the generated row. */
function jsonTextOf(item: Schema<"DatasetItem">): string {
  return JSON.stringify([item.input, item.expected_output, item.metadata]).toLowerCase();
}

it("searches the items of the latest version by their JSON text, case-insensitively and literally", async () => {
  server.use(...largeHandlers(createDataset({ seed: 1, now: NOW, traces: 50 })));
  const faq = named(generate(), "faq");
  const current = itemsAt(faq, faq.dataset.current_version);

  const refunds = await collect<Schema<"DatasetItem">>(
    `${PROJECT}/datasets/faq/items?q=REFUND&limit=7`,
  );
  const expected = current.filter((item) => jsonTextOf(item).includes("refund"));
  expect(expected.length).toBeGreaterThan(0);
  expect(refunds.map((row) => row.id)).toEqual(expected.map((row) => row.id));

  const underscores = await collect<Schema<"DatasetItem">>(`${PROJECT}/datasets/faq/items?q=_`);
  expect(underscores.map((row) => row.id)).toEqual(
    current.filter((item) => jsonTextOf(item).includes("_")).map((row) => row.id),
  );

  const percent = await collect<Schema<"DatasetItem">>(`${PROJECT}/datasets/faq/items?q=%25`);
  expect(percent).toHaveLength(0);
});

it("refuses a search that is blank or longer than 200 characters", async () => {
  server.use(...largeHandlers(createDataset({ seed: 1, now: NOW, traces: 50 })));
  const blank = await fetch(`${PROJECT}/datasets/faq/items?q=%20%20`);
  expect(blank.status).toBe(400);
  const long = await fetch(`${PROJECT}/datasets/faq/items?q=${"a".repeat(201)}`);
  expect(long.status).toBe(400);
  const ok = await fetch(`${PROJECT}/datasets/faq/items?q=${"a".repeat(200)}`);
  expect(ok.status).toBe(200);
});

it("lists the items a version introduced as added changes", async () => {
  server.use(...largeHandlers(createDataset({ seed: 1, now: NOW, traces: 50 })));
  const faq = named(generate(), "faq");
  const changes = await collect<Schema<"ItemChange">>(
    `${PROJECT}/datasets/faq/versions/1/changes?limit=5`,
  );
  const introduced = faq.history.filter((change) => change.version === 1);
  expect(changes.map((row) => row.item_id)).toEqual(introduced.map((row) => row.item_id));
  for (const row of changes) {
    expect(row.change).toBe("added");
    expect(row.after?.id).toBe(row.item_id);
    expect(row.before).toBeUndefined();
  }

  expect((await fetch(`${PROJECT}/datasets/faq/versions/0/changes`)).status).toBe(404);
  expect((await fetch(`${PROJECT}/datasets/faq/versions/99999/changes`)).status).toBe(404);
  expect((await fetch(`${PROJECT}/datasets/faq/versions/one/changes`)).status).toBe(400);
});

it("counts each version's changes from its history, and edits and deletes add no items", () => {
  const datasets = generate();
  const counted = { edited: 0, deleted: 0 };
  for (const generated of datasets) {
    for (const row of generated.versions) {
      const events = generated.history.filter((event) => event.version === row.version);
      expect(row.added).toBe(events.filter((event) => event.change === "added").length);
      expect(row.edited).toBe(events.filter((event) => event.change === "edited").length);
      expect(row.deleted).toBe(events.filter((event) => event.change === "deleted").length);
      if (row.edited + row.deleted > 0) {
        expect(row.added).toBe(0);
        expect(row.description).toMatch(/^(edit|delete) item /);
      }
      counted.edited += row.edited;
      counted.deleted += row.deleted;
    }
  }
  expect(counted.edited).toBeGreaterThan(0);
  expect(counted.deleted).toBeGreaterThan(0);
});

it("shows an edited item's old revision before the edit and its new one after", () => {
  const datasets = generate();
  const found = datasets.flatMap((generated) =>
    generated.history
      .filter((change) => change.change === "edited")
      .map((change) => ({ generated, change })),
  )[0];
  if (!found?.change.before || !found.change.after) {
    throw new Error("no edit in the generated datasets");
  }
  const { generated, change } = found;
  const { before, after } = change;
  expect(before?.version).toBeLessThan(change.version);
  expect(after?.version).toBe(change.version);
  expect(after?.id).toBe(before?.id);
  expect(itemsAt(generated, change.version - 1).map((row) => row.revision_id)).toContain(
    before?.revision_id,
  );
  const atEdit = itemsAt(generated, change.version).map((row) => row.revision_id);
  expect(atEdit).toContain(after?.revision_id);
  expect(atEdit).not.toContain(before?.revision_id);
  expect(generated.items.map((row) => row.revision_id)).toContain(after?.revision_id);
});

it("shows a deleted item in the versions before its delete, and not after or live", () => {
  const datasets = generate();
  const found = datasets.flatMap((generated) =>
    generated.history
      .filter((change) => change.change === "deleted")
      .map((change) => ({ generated, change })),
  )[0];
  if (!found) {
    throw new Error("no delete in the generated datasets");
  }
  const { generated, change } = found;
  expect(itemsAt(generated, change.version - 1).map((row) => row.id)).toContain(change.item_id);
  expect(itemsAt(generated, change.version).map((row) => row.id)).not.toContain(change.item_id);
  expect(generated.items.map((row) => row.id)).not.toContain(change.item_id);
});

it("keeps the live items as the items of the current version", () => {
  for (const generated of generate()) {
    expect(itemsAt(generated, generated.dataset.current_version).map((row) => row.id)).toEqual(
      generated.items.map((row) => row.id),
    );
  }
});

it("lists the items of an earlier version through the items endpoint", async () => {
  server.use(...largeHandlers(createDataset({ seed: 1, now: NOW, traces: 50 })));
  const generated = generate();
  const target = generated.find((item) =>
    item.history.some((change) => change.change === "deleted"),
  );
  const deleted = target?.history.find((change) => change.change === "deleted");
  if (!target || !deleted) {
    throw new Error("no delete in the generated datasets");
  }
  const before = await collect<Schema<"DatasetItem">>(
    `${PROJECT}/datasets/${target.dataset.id}/items?version=${deleted.version - 1}&limit=1000`,
  );
  const after = await collect<Schema<"DatasetItem">>(
    `${PROJECT}/datasets/${target.dataset.id}/items?version=${deleted.version}&limit=1000`,
  );
  expect(before.map((row) => row.id)).toContain(deleted.item_id);
  expect(after.map((row) => row.id)).not.toContain(deleted.item_id);
});

it("answers the latest experiments of each dataset with their score means", async () => {
  server.use(...largeHandlers(createDataset({ seed: 1, now: NOW, traces: 50 })));
  const datasets = generate();
  const withExperiments = datasets.find((item) => item.experiments.length > 0);
  const other = datasets.find((item) => item.dataset.id !== withExperiments?.dataset.id);
  if (!withExperiments || !other) {
    throw new Error("no dataset with an experiment to trend");
  }
  const unknown = "01900000-0000-7000-8000-00000000ffff";
  const response = await fetch(
    `${PROJECT}/dataset-trends?ids=${withExperiments.dataset.id},${unknown},${other.dataset.id}`,
  );
  expect(response.status).toBe(200);
  const body = (await response.json()) as {
    data: Schema<"DatasetTrend">[];
    scores_unavailable: boolean;
  };
  expect(body.scores_unavailable).toBe(false);
  expect(body.data.map((row) => row.dataset_id)).toEqual([
    withExperiments.dataset.id,
    other.dataset.id,
  ]);

  const trend = body.data[0];
  const newestFirst = [...withExperiments.experiments].reverse().slice(0, 10);
  expect(trend?.experiments.map((row) => row.id)).toEqual(
    newestFirst.map((row) => row.experiment.id),
  );
  const newest = newestFirst[0];
  const first = trend?.experiments[0];
  if (!newest || !first) {
    throw new Error("no trend experiment");
  }
  expect(first).toMatchObject({
    name: newest.experiment.name,
    status: newest.experiment.status,
    created_at: newest.experiment.created_at,
    result_count: newest.experiment.result_count,
    error_count: newest.summary.error_count,
  });
  expect(first.scores).toEqual(
    newest.summary.scores.map((score) => ({
      name: score.name,
      data_type: score.data_type,
      count: score.count,
      mean: score.data_type === "boolean" ? score.pass_rate : score.mean,
    })),
  );
});

it("refuses a bad, empty, repeated or oversized list of trend ids", async () => {
  server.use(...largeHandlers(createDataset({ seed: 1, now: NOW, traces: 50 })));
  const faq = named(generate(), "faq");
  expect((await fetch(`${PROJECT}/dataset-trends?ids=faq`)).status).toBe(400);
  expect((await fetch(`${PROJECT}/dataset-trends`)).status).toBe(400);
  expect(
    (await fetch(`${PROJECT}/dataset-trends?ids=${faq.dataset.id},${faq.dataset.id}`)).status,
  ).toBe(400);
  const many = Array.from(
    { length: 101 },
    (_, index) => `00000000-0000-7000-8000-${String(index + 1).padStart(12, "0")}`,
  ).join(",");
  expect((await fetch(`${PROJECT}/dataset-trends?ids=${many}`)).status).toBe(400);
});
