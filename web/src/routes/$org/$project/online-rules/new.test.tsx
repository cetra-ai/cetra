import { screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import createFixture from "../../../../../../fixtures/api/requests/online-rule-create.json";
import evaluatorsFixture from "../../../../../../fixtures/api/static/evaluators.json";

const RELEVANCE = evaluatorsFixture.data[0];
if (!RELEVANCE) {
  throw new Error("the evaluators fixture is empty");
}
const RELEVANCE_OPTION = "Relevance (LLM judge)";

/** Picks an option of the Radix select named `name`. */
async function choose(user: UserEvent, name: string, option: string) {
  await user.click(screen.getByRole("combobox", { name }));
  await user.click(await screen.findByRole("option", { name: option }));
}

/** The variable mapping rows, as `[variable, expression]` pairs in order. */
function mappingRows(): [string, string][] {
  return screen
    .queryAllByLabelText(/^Mapping variable \d+$/)
    .map((variable, index): [string, string] => [
      (variable as HTMLInputElement).value,
      (screen.getByLabelText(`Mapping expression ${index + 1}`) as HTMLInputElement).value,
    ]);
}

/** Records the body of the next create request and answers it with `answer`. */
function captureCreate(answer?: () => Response): { body: () => unknown; calls: () => number } {
  let body: unknown;
  let calls = 0;
  server.use(
    http.post("/api/v1/projects/:project/online-rules", async ({ request }) => {
      calls += 1;
      body = await request.json();
      if (answer) {
        return answer();
      }
      return HttpResponse.json(
        {
          ...(createFixture as Record<string, unknown>),
          id: "01900000-0000-7000-8000-000000000799",
        },
        { status: 201 },
      );
    }),
  );
  return { body: () => body, calls: () => calls };
}

it("creates a judge rule with a budget and opens the list", async () => {
  const created = captureCreate();
  const { user, router } = await renderApp("/acme/support-bot/online-rules/new");
  await screen.findByLabelText("Name");

  await user.type(screen.getByLabelText("Name"), "Relevance on answers");
  await choose(user, "Evaluator", RELEVANCE_OPTION);
  expect(mappingRows()).toEqual([
    ["input", "trace.input"],
    ["output", "trace.output"],
  ]);
  await user.type(screen.getByLabelText("Filter"), "status = 'ok'");
  await user.clear(screen.getByLabelText("Sample rate (%)"));
  await user.type(screen.getByLabelText("Sample rate (%)"), "25");
  await user.click(screen.getByRole("checkbox", { name: "Limit spend" }));
  await user.type(screen.getByLabelText("Amount (USD)"), "5");
  await user.click(screen.getByRole("button", { name: "Create rule" }));

  await waitFor(() => expect(created.body()).toEqual(createFixture));
  await waitFor(() =>
    expect(router.state.location.pathname).toBe("/acme/support-bot/online-rules"),
  );
});

it("refills the mapping with span expressions when the target changes", async () => {
  const { user } = await renderApp("/acme/support-bot/online-rules/new");
  await screen.findByLabelText("Name");

  await choose(user, "Evaluator", RELEVANCE_OPTION);
  expect(mappingRows()).toEqual([
    ["input", "trace.input"],
    ["output", "trace.output"],
  ]);

  await user.click(screen.getByRole("radio", { name: "Spans" }));
  expect(mappingRows()).toEqual([
    ["input", "span.input"],
    ["output", "span.output"],
  ]);
});

it("hides the budget for a check evaluator", async () => {
  server.use(
    http.get("/api/v1/projects/:project/evaluators", () =>
      HttpResponse.json({
        data: [
          {
            ...RELEVANCE,
            id: "01900000-0000-7000-8000-000000000501",
            name: "Exact match",
            kind: "check",
            config: { check: "exact_match" },
            output_score: { name: "exact", data_type: "boolean" },
          },
        ],
        next_cursor: null,
      }),
    ),
  );
  const { user } = await renderApp("/acme/support-bot/online-rules/new");
  await screen.findByLabelText("Name");

  await choose(user, "Evaluator", "Exact match (Check)");
  expect(screen.getByText("Budgets apply to LLM judge evaluators only.")).toBeInTheDocument();
  expect(screen.queryByRole("checkbox", { name: "Limit spend" })).toBeNull();
  expect(mappingRows()).toEqual([
    ["output", "trace.output"],
    ["expected", ""],
  ]);
});

it("shows field errors under their controls", async () => {
  captureCreate(() =>
    problemResponse(400, "bad_request", {
      errors: [
        { field: "name", message: "name must be 1 to 128 characters" },
        { field: "variable_mapping.input", message: "expression must be at most 1024 bytes" },
      ],
    }),
  );
  const { user } = await renderApp("/acme/support-bot/online-rules/new");
  await screen.findByLabelText("Name");

  await user.type(screen.getByLabelText("Name"), "Relevance");
  await choose(user, "Evaluator", RELEVANCE_OPTION);
  await user.click(screen.getByRole("button", { name: "Create rule" }));

  expect(await screen.findByText("name must be 1 to 128 characters")).toBeInTheDocument();
  expect(screen.getByText("expression must be at most 1024 bytes")).toBeInTheDocument();
});

it("shows an invalid filter with its position", async () => {
  captureCreate(() =>
    HttpResponse.json(
      {
        type: "about:blank",
        title: "Bad Request",
        status: 400,
        code: "invalid_filter",
        errors: [
          {
            field: "filter",
            message: "unknown column statusx",
            position: { line: 1, column: 8 },
          },
        ],
      },
      { status: 400, headers: { "Content-Type": "application/problem+json" } },
    ),
  );
  const { user } = await renderApp("/acme/support-bot/online-rules/new");
  await screen.findByLabelText("Name");

  await user.type(screen.getByLabelText("Name"), "Relevance");
  await choose(user, "Evaluator", RELEVANCE_OPTION);
  await user.type(screen.getByLabelText("Filter"), "statusx = 'ok'");
  await user.click(screen.getByRole("button", { name: "Create rule" }));

  expect(await screen.findByText("unknown column statusx (line 1, column 8)")).toBeInTheDocument();
});

it("shows the plan limit with its hint", async () => {
  captureCreate(() =>
    problemResponse(403, "plan_limit", {
      detail: "Your plan allows 1 active online rule.",
    }),
  );
  const { user } = await renderApp("/acme/support-bot/online-rules/new");
  await screen.findByLabelText("Name");

  await user.type(screen.getByLabelText("Name"), "Relevance");
  await choose(user, "Evaluator", RELEVANCE_OPTION);
  await user.click(screen.getByRole("button", { name: "Create rule" }));

  const alert = await screen.findByRole("alert");
  expect(within(alert).getByText("Your plan allows 1 active online rule.")).toBeInTheDocument();
  expect(
    within(alert).getByText(
      "A paused rule still counts as active: disable or delete a rule to free the slot.",
    ),
  ).toBeInTheDocument();
});

it("sends the sample rate as a fraction without float noise", async () => {
  const created = captureCreate();
  const { user } = await renderApp("/acme/support-bot/online-rules/new");
  await screen.findByLabelText("Name");

  await user.type(screen.getByLabelText("Name"), "Relevance");
  await choose(user, "Evaluator", RELEVANCE_OPTION);
  await user.clear(screen.getByLabelText("Sample rate (%)"));
  await user.type(screen.getByLabelText("Sample rate (%)"), "33.3");
  await user.click(screen.getByRole("button", { name: "Create rule" }));

  await waitFor(() => expect(created.body()).toMatchObject({ sample_rate: 0.333 }));
});

it("shows a name that is taken under the Name field", async () => {
  captureCreate(() =>
    problemResponse(409, "conflict", {
      detail: "an online rule with this name already exists",
    }),
  );
  const { user } = await renderApp("/acme/support-bot/online-rules/new");
  await screen.findByLabelText("Name");

  await user.type(screen.getByLabelText("Name"), "Relevance");
  await choose(user, "Evaluator", RELEVANCE_OPTION);
  await user.click(screen.getByRole("button", { name: "Create rule" }));

  expect(
    await screen.findByText("an online rule with this name already exists"),
  ).toBeInTheDocument();
  expect(screen.getByLabelText("Name")).toHaveAccessibleDescription(
    "an online rule with this name already exists",
  );
  expect(screen.queryByRole("alert")).toBeNull();
});
