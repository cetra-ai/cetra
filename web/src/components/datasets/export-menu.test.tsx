import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { ProjectProvider } from "@/components/shell/project-context";
import { server } from "@/mocks/node";
import projectFixture from "../../../../fixtures/api/static/project.json";
import { ExportMenu } from "./export-menu";

const DATASET_ID = "01900000-0000-7000-8000-000000000c01";
const EXPORT_PATH = "/api/v1/projects/:project/datasets/:dataset/export";
const HINT = "Open it in a spreadsheet as text: cells are not escaped against formulas.";

const originalCreate = URL.createObjectURL;
const originalRevoke = URL.revokeObjectURL;

beforeEach(() => {
  URL.createObjectURL = vi.fn(() => "blob:export");
  URL.revokeObjectURL = vi.fn();
});

afterEach(() => {
  URL.createObjectURL = originalCreate;
  URL.revokeObjectURL = originalRevoke;
  vi.restoreAllMocks();
});

function renderMenu(version: number) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
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
        <ExportMenu datasetId={DATASET_ID} version={version} />
      </ProjectProvider>
    </QueryClientProvider>,
  );
}

it("exports a version as CSV under the server's file name", async () => {
  const user = userEvent.setup();
  let requested: URLSearchParams | undefined;
  server.use(
    http.get(EXPORT_PATH, ({ request }) => {
      requested = new URL(request.url).searchParams;
      return new HttpResponse("id,input\n", {
        headers: {
          "Content-Type": "text/csv",
          "Content-Disposition": `attachment; filename="${DATASET_ID}-v3.csv"`,
        },
      });
    }),
  );
  const downloads: string[] = [];
  vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(function (
    this: HTMLAnchorElement,
  ) {
    downloads.push(this.download);
  });
  renderMenu(3);

  await user.click(screen.getByRole("button", { name: "Export" }));
  expect(await screen.findByText(HINT)).toBeInTheDocument();
  await user.click(screen.getByRole("menuitem", { name: "CSV" }));

  await waitFor(() => expect(downloads).toEqual([`${DATASET_ID}-v3.csv`]));
  expect(requested?.get("format")).toBe("csv");
  expect(requested?.get("version")).toBe("3");
});

it("disables the export while a download runs", async () => {
  const user = userEvent.setup();
  let release: () => void = () => undefined;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  server.use(
    http.get(EXPORT_PATH, async () => {
      await gate;
      return new HttpResponse("id\n", {
        headers: {
          "Content-Type": "text/csv",
          "Content-Disposition": `attachment; filename="${DATASET_ID}-v3.csv"`,
        },
      });
    }),
  );
  vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => undefined);
  renderMenu(3);

  await user.click(screen.getByRole("button", { name: "Export" }));
  await user.click(await screen.findByRole("menuitem", { name: "CSV" }));

  expect(await screen.findByRole("button", { name: "Exporting…" })).toBeDisabled();
  release();
  expect(await screen.findByRole("button", { name: "Export" })).toBeEnabled();
});
