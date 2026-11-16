import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { ProjectProvider } from "@/components/shell/project-context";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import datasetFixture from "../../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01.json";
import projectFixture from "../../../../fixtures/api/static/project.json";
import { DatasetDialog } from "./dataset-dialog";

// The fixture's schema is a plain object; the generated type is Record<string, never>.
const DATASET = datasetFixture as unknown as Schema<"Dataset">;
const OBJECT_SCHEMA = {
  type: "object",
} as unknown as Schema<"CreateDatasetRequest">["input_schema"];

function renderDialog(dataset?: Schema<"Dataset">) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const onSaved = vi.fn();
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
        <DatasetDialog open onOpenChange={onOpenChange} dataset={dataset} onSaved={onSaved} />
      </ProjectProvider>
    </QueryClientProvider>,
  );
  return { onSaved, onOpenChange };
}

/** Replaces the text of a textarea. Pasting keeps `{` and `[` literal for user-event. */
async function setText(user: ReturnType<typeof userEvent.setup>, label: string, text: string) {
  const field = screen.getByLabelText(label);
  await user.click(field);
  await user.clear(field);
  await user.paste(text);
}

it("creates a dataset with its schema", async () => {
  const user = userEvent.setup();
  let captured: unknown;
  server.use(
    http.post("/api/v1/projects/:project/datasets", async ({ request }) => {
      captured = await request.json();
      return HttpResponse.json({ ...DATASET, name: "golden" }, { status: 201 });
    }),
  );
  const { onSaved } = renderDialog();

  await user.type(screen.getByLabelText("Name"), "golden");
  await setText(user, "Input schema", '{"type":"object"}');
  await user.click(screen.getByRole("button", { name: "Create dataset" }));

  await waitFor(() => expect(onSaved).toHaveBeenCalledTimes(1));
  expect(captured).toEqual({ name: "golden", input_schema: { type: "object" } });
  expect(onSaved).toHaveBeenCalledWith(expect.objectContaining({ name: "golden" }));
});

it("shows a schema that is not JSON and sends nothing", async () => {
  const user = userEvent.setup();
  const { onSaved } = renderDialog();

  await user.type(screen.getByLabelText("Name"), "golden");
  await setText(user, "Input schema", "{");
  await user.click(screen.getByRole("button", { name: "Create dataset" }));

  expect(await screen.findByText("Not valid JSON")).toBeInTheDocument();
  expect(onSaved).not.toHaveBeenCalled();
});

it("shows the name conflict under Name", async () => {
  const user = userEvent.setup();
  server.use(
    http.post("/api/v1/projects/:project/datasets", () =>
      problemResponse(409, "conflict", { detail: "a dataset with this name already exists" }),
    ),
  );
  renderDialog();

  await user.type(screen.getByLabelText("Name"), "faq");
  await user.click(screen.getByRole("button", { name: "Create dataset" }));

  expect(await screen.findByText("A dataset with this name exists")).toBeInTheDocument();
});

it("lists the items that a new schema rejects", async () => {
  const user = userEvent.setup();
  server.use(
    http.patch("/api/v1/projects/:project/datasets/:dataset", () =>
      problemResponse(400, "validation_failed", {
        errors: [
          {
            field: "items[01900000-0000-7000-8000-000000000d01].input",
            message: '"q" is a required property',
          },
        ],
      }),
    ),
  );
  renderDialog(DATASET);

  await setText(user, "Input schema", '{"type":"string"}');
  await user.click(screen.getByRole("button", { name: "Save" }));

  expect(await screen.findByText('"q" is a required property')).toBeInTheDocument();
});

it("sends a cleared schema as null", async () => {
  const user = userEvent.setup();
  let captured: unknown;
  server.use(
    http.patch("/api/v1/projects/:project/datasets/:dataset", async ({ request }) => {
      captured = await request.json();
      return HttpResponse.json(DATASET);
    }),
  );
  const { onSaved } = renderDialog({ ...DATASET, input_schema: OBJECT_SCHEMA });

  await setText(user, "Input schema", "");
  await user.click(screen.getByRole("button", { name: "Save" }));

  await waitFor(() => expect(onSaved).toHaveBeenCalledTimes(1));
  expect(captured).toEqual({ input_schema: null });
});

it("keeps an edited form open on Escape, and the close button still closes it", async () => {
  const user = userEvent.setup();
  const { onOpenChange } = renderDialog();

  await user.type(screen.getByLabelText("Name"), "golden");
  await user.keyboard("{Escape}");
  expect(onOpenChange).not.toHaveBeenCalled();

  await user.click(screen.getByRole("button", { name: "Close" }));
  expect(onOpenChange).toHaveBeenCalledWith(false);
});
