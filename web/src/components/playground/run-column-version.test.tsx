import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ComponentProps } from "react";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { ProjectProvider } from "@/components/shell/project-context";
import type { Entry } from "@/lib/playground";
import llmConnections from "../../../../fixtures/api/static/llm-connections.json";
import projectFixture from "../../../../fixtures/api/static/project.json";
import { IDLE_RUN, RunColumn, versionLabel } from "./run-column";

const CONNECTION = "01900000-0000-7000-8000-000000000400";
const CONNECTIONS = llmConnections.data as Schema<"LlmConnection">[];
const TWO_VERSIONS = [
  { version: 2, labels: ["latest", "staging"] },
  { version: 1, labels: ["production"] },
];

type Props = ComponentProps<typeof RunColumn>;

/** Every prop of RunColumn: a pinned-capable column with the page's version list. */
function props(overrides: Partial<Props> = {}): Props {
  return {
    number: 1,
    column: { id: "c1", connectionId: CONNECTION, model: "", temperature: "", maxTokens: "" },
    run: IDLE_RUN,
    connections: CONNECTIONS,
    errors: {},
    runReason: undefined,
    shortcut: "Ctrl+Enter",
    canRemove: false,
    versions: TWO_VERSIONS,
    template: undefined,
    onChange: vi.fn(),
    onRun: vi.fn(),
    onStop: vi.fn(),
    onRemove: vi.fn(),
    onAddAnswer: vi.fn(),
    onPickVersion: vi.fn(),
    ...overrides,
  };
}

function renderColumn(overrides: Partial<Props> = {}) {
  render(
    <ProjectProvider
      value={{
        org: "acme",
        project: "support-bot",
        ref: "acme/support-bot",
        data: projectFixture as Schema<"Project">,
      }}
    >
      <RunColumn {...props(overrides)} />
    </ProjectProvider>,
  );
}

const TEMPLATE: Entry[] = [
  { role: "system", parts: [{ type: "text", content: "You are {{ product }}." }] },
  { type: "placeholder", name: "history" },
  { role: "user", parts: [{ type: "text", content: "{{ question }}" }] },
];

it("versionLabel gives v1 without labels and v2 · latest, staging with them", () => {
  expect(versionLabel({ version: 1, labels: [] })).toBe("v1");
  expect(versionLabel({ version: 2, labels: ["latest", "staging"] })).toBe("v2 · latest, staging");
});

it("hides the version select without versions", () => {
  renderColumn({ versions: undefined });
  expect(screen.queryByRole("combobox", { name: "Prompt version" })).not.toBeInTheDocument();
});

it("shows Draft for an unpinned column and the version for a pinned one", () => {
  renderColumn();
  expect(screen.getByRole("combobox", { name: "Prompt version" })).toHaveTextContent("Draft");
  renderColumn({
    number: 2,
    column: {
      id: "c2",
      connectionId: CONNECTION,
      model: "",
      temperature: "",
      maxTokens: "",
      version: 2,
    },
  });
  expect(screen.getAllByRole("combobox", { name: "Prompt version" })[1]).toHaveTextContent(
    "v2 · latest, staging",
  );
});

it("shows a pinned version missing from the list", () => {
  renderColumn({
    column: {
      id: "c1",
      connectionId: CONNECTION,
      model: "",
      temperature: "",
      maxTokens: "",
      version: 7,
    },
    versions: [
      { version: 2, labels: [] },
      { version: 1, labels: [] },
    ],
  });
  const trigger = screen.getByRole("combobox", { name: "Prompt version" });
  expect(trigger).toHaveTextContent("v7");
  expect(trigger).toHaveAttribute("title", "v7");
});

it("titles an unpinned column's version select Draft", () => {
  renderColumn();
  expect(screen.getByRole("combobox", { name: "Prompt version" })).toHaveAttribute(
    "title",
    "Draft",
  );
});

it("picking a version calls onPickVersion with its number, and Draft with undefined", async () => {
  const user = userEvent.setup();
  const onPickVersion = vi.fn();
  // Starts pinned to v2: picking the value a select already shows is no change.
  renderColumn({
    onPickVersion,
    column: {
      id: "c1",
      connectionId: CONNECTION,
      model: "",
      temperature: "",
      maxTokens: "",
      version: 2,
    },
  });

  await user.click(screen.getByRole("combobox", { name: "Prompt version" }));
  await user.click(await screen.findByRole("option", { name: "v1 · production" }));
  expect(onPickVersion).toHaveBeenLastCalledWith("c1", 1);

  await user.click(screen.getByRole("combobox", { name: "Prompt version" }));
  await user.click(await screen.findByRole("option", { name: "Draft" }));
  expect(onPickVersion).toHaveBeenLastCalledWith("c1", undefined);
});

it("shows the pinned template", () => {
  renderColumn({
    column: {
      id: "c1",
      connectionId: CONNECTION,
      model: "",
      temperature: "",
      maxTokens: "",
      version: 2,
    },
    template: TEMPLATE,
  });
  const list = within(screen.getByRole("list", { name: "Template of version 2" }));
  expect(list.getAllByRole("listitem")).toHaveLength(3);
  expect(list.getByText("Placeholder history")).toBeInTheDocument();
  expect(list.getByText("You are {{ product }}.")).toBeInTheDocument();
});

it("shows no template for a draft column", () => {
  renderColumn({ template: TEMPLATE });
  expect(screen.queryByRole("list", { name: /Template of version/ })).not.toBeInTheDocument();
});

it("ends a long version label in an ellipsis and shows it whole in the title", () => {
  renderColumn({
    column: {
      id: "c1",
      connectionId: CONNECTION,
      model: "",
      temperature: "",
      maxTokens: "",
      version: 2,
    },
  });
  const trigger = screen.getByRole("combobox", { name: "Prompt version" });
  expect(trigger).toHaveAttribute("title", "v2 · latest, staging");
  expect(within(trigger).getByText("v2 · latest, staging")).toHaveClass("truncate");
});
