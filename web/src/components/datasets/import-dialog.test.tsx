import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { ProjectProvider } from "@/components/shell/project-context";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import projectFixture from "../../../../fixtures/api/static/project.json";
import { ImportDialog } from "./import-dialog";

const DATASET_ID = "01900000-0000-7000-8000-000000000c01";
const PREVIEW_PATH = "/api/v1/projects/:project/datasets/:dataset/import/preview";
const IMPORT_PATH = "/api/v1/projects/:project/datasets/:dataset/import";

const CSV_PREVIEW: Schema<"ImportPreview"> = {
  format: "csv",
  columns: ["question", "answer"],
  rows: [["What?", "That"]],
  errors: [],
  truncated: false,
};

/** The classes of `node:buffer` (the module name is kept in a variable: no Node types here). */
type NodeBuffer = { File: typeof File; Blob: typeof Blob };
const NODE_BUFFER = "node:buffer";

/**
 * jsdom's FormData, File and Blob cannot be sent through the test's fetch: the body length no
 * longer matches, and undici's parser rejects the parts. These tests use Node's File and Blob
 * (the fetch's own) and undici's FormData, so the request is the one a browser sends.
 */
beforeEach(async () => {
  const { File: NodeFile, Blob: NodeBlob } = (await import(NODE_BUFFER)) as NodeBuffer;
  vi.stubGlobal("File", NodeFile);
  vi.stubGlobal("Blob", NodeBlob);
  const sample = await new Request("http://localhost/", {
    method: "POST",
    headers: { "Content-Type": "multipart/form-data; boundary=probe" },
    body: "--probe--\r\n",
  }).formData();
  vi.stubGlobal("FormData", sample.constructor);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function renderDialog() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const onOpenChange = vi.fn();
  render(
    <QueryClientProvider client={queryClient}>
      <ProjectProvider
        value={{
          org: "acme",
          project: "support-bot",
          ref: "acme/support-bot",
          data: projectFixture as Schema<"Project">,
        }}
      >
        <ImportDialog datasetId={DATASET_ID} open onOpenChange={onOpenChange} />
      </ProjectProvider>
    </QueryClientProvider>,
  );
  return { onOpenChange };
}

async function choose(user: ReturnType<typeof userEvent.setup>, file: File) {
  await user.upload(screen.getByLabelText("Choose a CSV or JSONL file"), file);
}

function csvFile() {
  return new File(["question,answer\nWhat?,That\n"], "faq.csv", { type: "text/csv" });
}

it("previews a CSV file and imports it with the mapping", async () => {
  const user = userEvent.setup();
  let previewed: { format: string | null; partial: string | null; body: string } | undefined;
  let imported: { file: string | undefined; mapping: unknown } | undefined;
  server.use(
    http.post(PREVIEW_PATH, async ({ request }) => {
      previewed = {
        format: new URL(request.url).searchParams.get("format"),
        partial: new URL(request.url).searchParams.get("partial"),
        body: await request.text(),
      };
      return HttpResponse.json(CSV_PREVIEW);
    }),
    http.post(IMPORT_PATH, async ({ request }) => {
      const form = await request.formData();
      const mapping = form.get("mapping");
      imported = {
        file: (form.get("file") as File | null)?.name,
        mapping: JSON.parse(mapping instanceof Blob ? await mapping.text() : "null"),
      };
      return HttpResponse.json(
        { version: 2, imported: 1, error_count: 0, errors: [] },
        { status: 201 },
      );
    }),
  );
  renderDialog();

  await choose(user, csvFile());
  expect(await screen.findByRole("table", { name: "Preview" })).toBeInTheDocument();
  expect(previewed?.format).toBe("csv");
  expect(previewed?.partial).toBe("false");
  expect(previewed?.body.startsWith("question,answer")).toBe(true);

  await user.click(screen.getByRole("combobox", { name: "Target of answer" }));
  await user.click(await screen.findByRole("option", { name: "Whole expected output" }));
  await user.click(screen.getByRole("button", { name: "Import" }));

  expect(await screen.findByText("Imported 1 items as version 2.")).toBeInTheDocument();
  expect(imported?.file).toBe("faq.csv");
  expect(imported?.mapping).toEqual({
    columns: [
      { column: "question", target: "input.question" },
      { column: "answer", target: "expected" },
    ],
  });
});

it("imports JSONL without a mapping", async () => {
  const user = userEvent.setup();
  let hadMapping: boolean | undefined;
  server.use(
    http.post(PREVIEW_PATH, () =>
      HttpResponse.json({
        format: "jsonl",
        columns: ["input"],
        rows: [{ input: "What?" }],
        errors: [],
        truncated: false,
      }),
    ),
    http.post(IMPORT_PATH, async ({ request }) => {
      hadMapping = (await request.formData()).has("mapping");
      return HttpResponse.json(
        { version: 2, imported: 1, error_count: 0, errors: [] },
        { status: 201 },
      );
    }),
  );
  renderDialog();

  await choose(user, new File(['{"input":"What?"}\n'], "faq.jsonl", { type: "application/jsonl" }));
  await user.click(await screen.findByRole("button", { name: "Import" }));

  expect(await screen.findByText("Imported 1 items as version 2.")).toBeInTheDocument();
  expect(hadMapping).toBe(false);
});

it("refuses another extension", async () => {
  // The file input's accept list would filter the upload before the wizard sees it.
  const user = userEvent.setup({ applyAccept: false });
  renderDialog();

  await choose(user, new File(["{}"], "data.json", { type: "application/json" }));

  expect(await screen.findByText("Choose a .csv or .jsonl file")).toBeInTheDocument();
});

it("blocks an import without an input", async () => {
  const user = userEvent.setup();
  server.use(http.post(PREVIEW_PATH, () => HttpResponse.json(CSV_PREVIEW)));
  renderDialog();

  await choose(user, csvFile());
  await screen.findByRole("table", { name: "Mapping" });
  await user.click(screen.getByRole("combobox", { name: "Target of question" }));
  await user.click(await screen.findByRole("option", { name: "Ignore" }));
  await user.click(screen.getByRole("combobox", { name: "Target of answer" }));
  await user.click(await screen.findByRole("option", { name: "Ignore" }));

  expect(await screen.findByText("Map at least one column to the input")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Import" })).toBeDisabled();
});

it("lists the row errors", async () => {
  const user = userEvent.setup();
  const errors = Array.from({ length: 100 }, (_, index) => ({
    row: index + 2,
    message: "the row has no input",
  }));
  server.use(
    http.post(PREVIEW_PATH, () => HttpResponse.json(CSV_PREVIEW)),
    http.post(IMPORT_PATH, () =>
      HttpResponse.json({ version: 3, imported: 1, error_count: 150, errors }, { status: 201 }),
    ),
  );
  renderDialog();

  await choose(user, csvFile());
  await user.click(await screen.findByRole("button", { name: "Import" }));

  expect(await screen.findByText("150 rows were skipped.")).toBeInTheDocument();
  expect(screen.getByText("Showing the first 100 errors.")).toBeInTheDocument();
  const table = screen.getByRole("table", { name: "Row errors" });
  expect(within(table).getAllByRole("row").slice(1)).toHaveLength(100);
});

it("shows a header error from the preview", async () => {
  const user = userEvent.setup();
  server.use(
    http.post(PREVIEW_PATH, () =>
      problemResponse(400, "invalid_request", {
        detail: "the header has a duplicate column name",
      }),
    ),
  );
  renderDialog();

  await choose(user, csvFile());

  expect(await screen.findByText("the header has a duplicate column name")).toBeInTheDocument();
});

it("lists the rows of a refused import", async () => {
  const user = userEvent.setup();
  server.use(
    http.post(PREVIEW_PATH, () => HttpResponse.json(CSV_PREVIEW)),
    http.post(IMPORT_PATH, () =>
      problemResponse(400, "invalid_request", {
        detail: "no row could be imported",
        errors: [{ field: "rows[1]", message: "the row has no input" }],
      }),
    ),
  );
  renderDialog();

  await choose(user, csvFile());
  await user.click(await screen.findByRole("button", { name: "Import" }));

  expect(await screen.findByText("no row could be imported")).toBeInTheDocument();
  const table = screen.getByRole("table", { name: "Row errors" });
  const row = within(table).getAllByRole("row")[1] as HTMLElement;
  expect(within(row).getByText("1")).toBeInTheDocument();
  expect(within(row).getByText("the row has no input")).toBeInTheDocument();
});
