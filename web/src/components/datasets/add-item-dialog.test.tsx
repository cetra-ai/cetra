import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { ProjectProvider } from "@/components/shell/project-context";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import projectFixture from "../../../../fixtures/api/static/project.json";
import { AddItemDialog } from "./add-item-dialog";

const DATASET_ID = "01900000-0000-7000-8000-000000000c01";
const PATH = "/api/v1/projects/:project/datasets/:dataset/items";

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
        <AddItemDialog datasetId={DATASET_ID} open onOpenChange={onOpenChange} />
      </ProjectProvider>
    </QueryClientProvider>,
  );
  return { onOpenChange };
}

/** Replaces the text of a textarea. Pasting keeps `{` and `[` literal for user-event. */
async function setText(user: ReturnType<typeof userEvent.setup>, label: string, text: string) {
  const field = screen.getByRole("textbox", { name: label });
  await user.click(field);
  await user.paste(text);
}

it("adds one item", async () => {
  const user = userEvent.setup();
  let captured: unknown;
  server.use(
    http.post(PATH, async ({ request }) => {
      captured = await request.json();
      return HttpResponse.json({ version: 3, item_ids: [DATASET_ID] }, { status: 201 });
    }),
  );
  const { onOpenChange } = renderDialog();

  await setText(user, "Input", '{"q":"hi"}');
  await setText(user, "Expected output", '"hello"');
  await user.click(screen.getByRole("button", { name: "Add item" }));

  await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
  expect(captured).toEqual({ items: [{ input: { q: "hi" }, expected_output: "hello" }] });
});

it("maps the batch field errors to the fields", async () => {
  const user = userEvent.setup();
  server.use(
    http.post(PATH, () =>
      problemResponse(400, "validation_failed", {
        errors: [{ field: "items[0].input", message: '"q" is a required property' }],
      }),
    ),
  );
  renderDialog();

  await setText(user, "Input", "{}");
  await user.click(screen.getByRole("button", { name: "Add item" }));

  expect(await screen.findByText('"q" is a required property')).toBeInTheDocument();
  expect(screen.getByRole("textbox", { name: "Input" }).parentElement).toContainElement(
    screen.getByText('"q" is a required property'),
  );
});

it("shows the item cap", async () => {
  const user = userEvent.setup();
  server.use(
    http.post(PATH, () =>
      problemResponse(409, "conflict", {
        detail: "a dataset holds at most 50000 items",
      }),
    ),
  );
  renderDialog();

  await setText(user, "Input", "{}");
  await user.click(screen.getByRole("button", { name: "Add item" }));

  expect(await screen.findByText("a dataset holds at most 50000 items")).toBeInTheDocument();
});
