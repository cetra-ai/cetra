import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { DefinitionsPicker } from "@/components/annotation-queues/definitions-picker";
import { ProjectProvider } from "@/components/shell/project-context";
import { ANNOTATION_DEFINITIONS, serveAnnotationDefinitions } from "@/test/score-definitions";
import projectFixture from "../../../../fixtures/api/static/project.json";

const HELPFULNESS = "01900000-0000-7000-8000-000000000601";
const TONE = "01900000-0000-7000-8000-000000000603";
const RESOLVED = "01900000-0000-7000-8000-000000000602";

function renderPicker(value: readonly string[], onChange = vi.fn()) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
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
        <DefinitionsPicker value={value} onChange={onChange} />
      </ProjectProvider>
    </QueryClientProvider>,
  );
  return { onChange };
}

/** `count` numeric definitions, named def-00 and up, for the cap. */
function manyDefinitions(count: number): Schema<"ScoreDefinition">[] {
  const base = ANNOTATION_DEFINITIONS[0];
  if (base === undefined) {
    throw new Error("the fixture has no definitions");
  }
  return Array.from({ length: count }, (_, index) => ({
    ...base,
    id: `01900000-0000-7000-8000-${String(index).padStart(12, "0")}`,
    name: `def-${String(index).padStart(2, "0")}`,
  }));
}

it("checks a definition and appends it to the value", async () => {
  const user = userEvent.setup();
  serveAnnotationDefinitions();
  const { onChange } = renderPicker([HELPFULNESS]);

  await user.click(await screen.findByRole("checkbox", { name: "resolved" }));

  expect(onChange).toHaveBeenCalledWith([HELPFULNESS, RESOLVED]);
});

it("moves a checked definition up and down by id", async () => {
  const user = userEvent.setup();
  serveAnnotationDefinitions();
  const { onChange } = renderPicker([TONE, HELPFULNESS]);

  expect(await screen.findByRole("button", { name: "Move tone up" })).toBeDisabled();
  await user.click(screen.getByRole("button", { name: "Move helpfulness up" }));

  expect(onChange).toHaveBeenLastCalledWith([HELPFULNESS, TONE]);
});

it("lists the checked definitions first, in the value's order, then the rest by name", async () => {
  serveAnnotationDefinitions();
  renderPicker([TONE, HELPFULNESS]);

  const list = await screen.findByRole("list");
  const names = within(list)
    .getAllByRole("listitem")
    .map((item) => item.querySelector("label")?.textContent);
  expect(names).toEqual(["tone", "helpfulness", "note", "resolved"]);
});

it("disables the unchecked definitions once a queue holds fifty", async () => {
  const definitions = manyDefinitions(51);
  const value = definitions.slice(0, 50).map((definition) => definition.id);
  serveAnnotationDefinitions(definitions);
  renderPicker(value);

  expect(await screen.findByText("A queue holds at most 50 definitions.")).toBeInTheDocument();
  expect(screen.getByRole("checkbox", { name: "def-50" })).toBeDisabled();
  expect(screen.getByRole("checkbox", { name: "def-00" })).toBeEnabled();
});
