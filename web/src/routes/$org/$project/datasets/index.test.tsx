import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import datasetsFixture from "../../../../../../fixtures/api/responses/datasets.json";
import projectFixture from "../../../../../../fixtures/api/static/project.json";

function firstDataset() {
  const dataset = datasetsFixture.data[0];
  if (!dataset) {
    throw new Error("the datasets fixture has no dataset");
  }
  return dataset;
}

it("lists the datasets with their items, version and experiments", async () => {
  const dataset = firstDataset();
  await renderApp("/acme/support-bot/datasets");

  const table = await screen.findByRole("table", { name: "Datasets" });
  const row = within(table).getByText(dataset.name).closest("tr");
  if (!row) {
    throw new Error(`no row named ${dataset.name}`);
  }
  expect(within(row).getByRole("link", { name: dataset.name })).toHaveAttribute(
    "href",
    `/acme/support-bot/datasets/${dataset.id}`,
  );
  expect(within(row).getByText(String(dataset.item_count))).toBeInTheDocument();
  expect(within(row).getByText(`v${dataset.current_version}`)).toBeInTheDocument();
  expect(within(row).getByText(String(dataset.experiment_count))).toBeInTheDocument();
});

it("shows the empty state when the project has no datasets", async () => {
  server.use(
    http.get("/api/v1/projects/:project/datasets", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );

  await renderApp("/acme/support-bot/datasets");

  expect(await screen.findByText("No datasets")).toBeInTheDocument();
  expect(
    screen.getByText("Create one, import a CSV or JSONL file, or add items from a trace."),
  ).toBeInTheDocument();
  expect(screen.queryByRole("table", { name: "Datasets" })).toBeNull();
});

it("searches by name, sending the query after a pause", async () => {
  const user = userEvent.setup();
  const queries: (string | null)[] = [];
  server.use(
    http.get("/api/v1/projects/:project/datasets", ({ request }) => {
      queries.push(new URL(request.url).searchParams.get("q"));
      return HttpResponse.json(datasetsFixture);
    }),
  );

  await renderApp("/acme/support-bot/datasets");
  await screen.findByRole("table", { name: "Datasets" });
  await user.type(screen.getByRole("textbox", { name: "Search datasets" }), "faq");

  await waitFor(() => expect(queries).toContain("faq"));
});

it("opens a new dataset after creating it", async () => {
  const user = userEvent.setup();
  server.use(
    http.post("/api/v1/projects/:project/datasets", () =>
      HttpResponse.json({ ...firstDataset(), name: "golden" }, { status: 201 }),
    ),
  );

  const { router } = await renderApp("/acme/support-bot/datasets");
  await screen.findByRole("table", { name: "Datasets" });
  await user.click(screen.getByRole("button", { name: "New dataset" }));
  await user.type(screen.getByLabelText("Name"), "golden");
  await user.click(screen.getByRole("button", { name: "Create dataset" }));

  await waitFor(() =>
    expect(router.state.location.pathname).toBe(`/acme/support-bot/datasets/${firstDataset().id}`),
  );
});

it("disables New dataset for viewers, with the reason", async () => {
  server.use(
    http.get("/api/v1/projects/:project", () =>
      HttpResponse.json({ ...projectFixture, role: "viewer" }),
    ),
  );

  await renderApp("/acme/support-bot/datasets");

  await screen.findByRole("table", { name: "Datasets" });
  const button = screen.getByRole("button", { name: "New dataset" });
  expect(button).toBeDisabled();
  expect(button.parentElement).toHaveAccessibleDescription("Only members can create datasets");
});

it("follows the search text when the URL changes", async () => {
  const { router } = await renderApp("/acme/support-bot/datasets?q=faq");

  const search = await screen.findByRole("textbox", { name: "Search datasets" });
  expect(search).toHaveValue("faq");
  await act(async () => {
    await router.navigate({
      to: "/$org/$project/datasets",
      params: { org: "acme", project: "support-bot" },
      search: { q: "golden" },
    });
  });

  await waitFor(() => expect(search).toHaveValue("golden"));
});
