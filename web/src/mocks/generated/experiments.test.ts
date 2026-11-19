import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { experimentHandlers } from "@/mocks/generated/experiments";
import { server } from "@/mocks/node";

const PROJECT = `${location.origin}/api/v1/projects/acme%2Fsupport-bot`;
const FAQ = "01900000-0000-7000-8000-000000000c01";
const LEGACY = "support-legacy";

function install(): void {
  server.use(...experimentHandlers());
}

async function getJson<T>(path: string): Promise<{ status: number; body: T }> {
  const response = await fetch(`${PROJECT}${path}`);
  return { status: response.status, body: (await response.json()) as T };
}

type ExperimentPage = { data: Schema<"Experiment">[]; next_cursor: string | null };
type Results = { rows: Schema<"ResultRow">[]; next_cursor: string | null };

it("lists only the generated experiments, newest first", async () => {
  install();
  const { body } = await getJson<ExperimentPage>("/experiments?limit=50");
  expect(body.data).toHaveLength(16);
  const times = body.data.map((row) => row.created_at);
  expect([...times].sort().reverse()).toEqual(times);
  expect(body.data.some((row) => row.id.endsWith("e01") || row.id.endsWith("e02"))).toBe(false);
});

it("filters the list by dataset id or by the name of a deleted dataset", async () => {
  install();
  const faq = await getJson<ExperimentPage>(`/experiments?dataset=${FAQ}&limit=50`);
  expect(faq.body.data).toHaveLength(12);
  const legacy = await getJson<ExperimentPage>(`/experiments?dataset=${LEGACY}&limit=50`);
  expect(legacy.body.data).toHaveLength(4);
  expect(legacy.body.data.every((row) => row.dataset_id === undefined)).toBe(true);
});

it("pages the list with a cursor", async () => {
  install();
  const first = await getJson<ExperimentPage>("/experiments?limit=10");
  expect(first.body.data).toHaveLength(10);
  expect(first.body.next_cursor).not.toBeNull();
  const second = await getJson<ExperimentPage>(
    `/experiments?limit=10&cursor=${encodeURIComponent(first.body.next_cursor ?? "")}`,
  );
  expect(second.body.data).toHaveLength(6);
  expect(second.body.next_cursor).toBeNull();
});

it("has one running experiment with partial results", async () => {
  install();
  const { body } = await getJson<ExperimentPage>(`/experiments?dataset=${FAQ}&limit=50`);
  const running = body.data.filter((row) => row.status === "running");
  expect(running).toHaveLength(1);
  expect(running[0]?.result_count).toBe(18);
  expect(running[0]?.item_count).toBe(30);
  expect(running[0]?.finished_at).toBeUndefined();
});

it("pages the results of an experiment, and filters errors", async () => {
  install();
  const { body: list } = await getJson<ExperimentPage>(`/experiments?dataset=${FAQ}&limit=50`);
  const id = list.data[1]?.id ?? "";
  const seen: string[] = [];
  let cursor: string | null = null;
  do {
    const query: string = cursor ? `&cursor=${encodeURIComponent(cursor)}` : "";
    const page: { body: Results } = await getJson<Results>(
      `/experiments/${id}/results?limit=7${query}`,
    );
    const body: Results = page.body;
    seen.push(...body.rows.map((row) => `${row.dataset_item_id}:${row.repetition}`));
    cursor = body.next_cursor;
  } while (cursor !== null);
  expect(new Set(seen).size).toBe(seen.length);
  expect(seen).toHaveLength(30);

  const errors = await getJson<Results>(`/experiments/${id}/results?errors_only=true&limit=200`);
  expect(errors.body.rows.every((row) => row.error !== undefined)).toBe(true);
});

it("answers the summaries in request order and leaves out unknown ids", async () => {
  install();
  const { body: list } = await getJson<ExperimentPage>(`/experiments?dataset=${FAQ}&limit=50`);
  const [first, second] = [list.data[0]?.id ?? "", list.data[1]?.id ?? ""];
  const unknown = "01900000-0000-7000-8000-0000000fffff";
  const { body } = await getJson<{
    data: Schema<"ExperimentSummary">[];
    skipped: string[];
  }>(`/experiments/summaries?ids=${second},${unknown},${first}`);
  expect(body.data.map((row) => row.experiment.id)).toEqual([second, first]);
  expect(body.skipped).toEqual([]);
});

it("refuses 26 summary ids", async () => {
  install();
  const ids = Array.from(
    { length: 26 },
    (_, n) => `01900000-0000-7000-8000-${n.toString(16).padStart(12, "0")}`,
  );
  const { body: list } = await getJson<ExperimentPage>(`/experiments?dataset=${FAQ}&limit=50`);
  ids[0] = list.data[0]?.id ?? "";
  const { status } = await getJson(`/experiments/summaries?ids=${ids.join(",")}`);
  expect(status).toBe(400);
});

it("keeps the fixture experiment that the mock does not know", async () => {
  install();
  const { body } = await getJson<Schema<"Experiment">>(
    "/experiments/01900000-0000-7000-8000-000000000e01",
  );
  expect(body.name).toBe("baseline");
});

it("counts the regressed and improved items the rows show", async () => {
  install();
  const { body: list } = await getJson<ExperimentPage>(`/experiments?dataset=${FAQ}&limit=50`);
  const ids = list.data.slice(0, 3).map((row) => row.id);
  const all = await getJson<Schema<"ExperimentComparison">>(
    `/experiments/compare?ids=${ids.join(",")}&limit=200`,
  );
  expect(all.status).toBe(200);
  const rows = all.body.rows;
  for (const [index, delta] of all.body.deltas.entries()) {
    const experiment = ids[index + 1];
    const regressed = rows.filter((row) =>
      row.cells.some((cell) => cell.experiment_id === experiment && cell.regressed),
    );
    const improved = rows.filter((row) =>
      row.cells.some((cell) => cell.experiment_id === experiment && cell.improved),
    );
    expect(delta.regressed_items).toBe(regressed.length);
    expect(delta.improved_items).toBe(improved.length);
  }

  const only = await getJson<Schema<"ExperimentComparison">>(
    `/experiments/compare?ids=${ids.join(",")}&only=regressed&limit=200`,
  );
  for (const row of only.body.rows) {
    expect(row.cells.some((cell) => cell.regressed)).toBe(true);
  }
});
