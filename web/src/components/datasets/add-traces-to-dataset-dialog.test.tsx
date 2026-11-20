import { screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import datasetFixture from "../../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01.json";
import fromTracesFixture from "../../../../fixtures/api/static/dataset-items-from-traces.json";
import issueFixture from "../../../../fixtures/api/static/issue.json";
import eventsFixture from "../../../../fixtures/api/static/issue-events.json";
import occurrencesFixture from "../../../../fixtures/api/static/issue-occurrences.json";
import { addTracesReason } from "./add-traces-to-dataset-dialog";

const FROM_TRACES = "/api/v1/projects/:project/datasets/:dataset/items/from-traces";
const ISSUE_ITEMS = "/api/v1/projects/:project/issues/:id/dataset-items";
const FIRST = "a1000000000000000000000000000006";
const SECOND = "a1000000000000000000000000000004";
const TRACES_URL = "/acme/support-bot/traces";
const ISSUE_URL = `/acme/support-bot/issues/${issueFixture.id}`;
const FAQ_ID = "01900000-0000-7000-8000-000000000c01";

/** Answers each from-traces request with 201 and records the body and the dataset. */
function captureTraces(): { bodies: unknown[]; datasets: string[] } {
  const bodies: unknown[] = [];
  const datasets: string[] = [];
  server.use(
    http.post(FROM_TRACES, async ({ request, params }) => {
      bodies.push(await request.json());
      datasets.push(String(params.dataset));
      return HttpResponse.json(fromTracesFixture, { status: 201 });
    }),
  );
  return { bodies, datasets };
}

/** Checks the first `count` rows of the traces list. */
async function checkRows(user: UserEvent, count: number) {
  const grid = await screen.findByRole("grid", { name: "Traces" });
  const boxes = within(grid).getAllByRole("checkbox", { name: "Select row" });
  for (const box of boxes.slice(0, count)) {
    await user.click(box);
  }
}

/** Checks two rows and opens the dialog from the toolbar. */
async function openWithTwoRows() {
  const rendered = await renderApp(TRACES_URL);
  await checkRows(rendered.user, 2);
  await rendered.user.click(await screen.findByRole("button", { name: "Add 2 to dataset" }));
  return { ...rendered, dialog: await screen.findByRole("dialog") };
}

/** Picks an option of the Radix select named `name`, inside the dialog. */
async function choose(user: UserEvent, dialog: HTMLElement, name: string, option: string) {
  await user.click(within(dialog).getByRole("combobox", { name }));
  await user.click(await screen.findByRole("option", { name: option }));
}

describe("addTracesReason", () => {
  it("allows members up to 100 traces", () => {
    expect(addTracesReason("member", 1)).toBeUndefined();
    expect(addTracesReason("admin", 100)).toBeUndefined();
  });

  it("refuses viewers and users without a role", () => {
    expect(addTracesReason("viewer", 1)).toBe("Only members can add to datasets");
    expect(addTracesReason(undefined, 1)).toBe("Only members can add to datasets");
  });

  it("refuses more than 100 traces, after the role check", () => {
    expect(addTracesReason("member", 101)).toBe("At most 100 traces at once");
    expect(addTracesReason("viewer", 101)).toBe("Only members can add to datasets");
  });
});

describe("add traces to dataset dialog", () => {
  it("adds the checked traces in one request and reports the result", async () => {
    const { bodies, datasets } = captureTraces();
    const { user, dialog } = await openWithTwoRows();

    expect(
      within(dialog).getByRole("heading", { name: "Add 2 traces to a dataset" }),
    ).toBeInTheDocument();
    await choose(user, dialog, "Dataset", "faq");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    expect(await within(dialog).findByText("Added 1 item to faq.")).toBeInTheDocument();
    expect(bodies).toEqual([{ items: [{ trace_id: FIRST }, { trace_id: SECOND }] }]);
    expect(datasets).toEqual(["01900000-0000-7000-8000-000000000c01"]);
    expect(within(dialog).getByRole("link", { name: "Open dataset" })).toBeInTheDocument();
  });

  it("lists the traces that were not copied", async () => {
    captureTraces();
    const { user, dialog } = await openWithTwoRows();

    await choose(user, dialog, "Dataset", "faq");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    expect(await within(dialog).findByText("1 trace was not copied")).toBeInTheDocument();
    expect(within(dialog).getByText("a1000000")).toBeInTheDocument();
    expect(within(dialog).getByText("the trace or span was not found")).toBeInTheDocument();
  });

  it("clears the checked traces once they are added", async () => {
    captureTraces();
    const { user, dialog } = await openWithTwoRows();

    await choose(user, dialog, "Dataset", "faq");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));
    await within(dialog).findByText("Added 1 item to faq.");

    await waitFor(() =>
      expect(
        screen.queryByRole("button", { name: "Add 2 to dataset", hidden: true }),
      ).not.toBeInTheDocument(),
    );
  });

  it("creates a dataset before adding the traces", async () => {
    const order: string[] = [];
    server.use(
      http.post("/api/v1/projects/:project/datasets", async ({ request }) => {
        const body = (await request.json()) as { name: string };
        order.push(`create ${body.name}`);
        return HttpResponse.json(
          { ...datasetFixture, id: "01900000-0000-7000-8000-000000000c09", name: body.name },
          { status: 201 },
        );
      }),
      http.post(FROM_TRACES, ({ params }) => {
        order.push(`items ${String(params.dataset)}`);
        return HttpResponse.json(fromTracesFixture, { status: 201 });
      }),
    );
    const { user, dialog } = await openWithTwoRows();

    await choose(user, dialog, "Dataset", "New dataset…");
    await user.type(
      within(dialog).getByRole("textbox", { name: "Name of the new dataset" }),
      "golden",
    );
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    expect(await within(dialog).findByText("Added 1 item to golden.")).toBeInTheDocument();
    expect(order).toEqual(["create golden", "items 01900000-0000-7000-8000-000000000c09"]);
  });

  it("shows a 400 field error in the dialog and keeps the checked traces", async () => {
    server.use(
      http.post(FROM_TRACES, () =>
        problemResponse(400, "validation_failed", {
          errors: [{ field: "items[0]", message: "the trace is too large to copy" }],
        }),
      ),
    );
    const { user, dialog } = await openWithTwoRows();

    await choose(user, dialog, "Dataset", "faq");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    expect(await within(dialog).findByRole("alert")).toHaveTextContent(
      "the trace is too large to copy",
    );
    expect(within(dialog).queryByRole("link", { name: "Open dataset" })).not.toBeInTheDocument();
  });

  it("shows the server's message when storage is busy", async () => {
    server.use(
      http.post(FROM_TRACES, () =>
        problemResponse(503, "busy", { detail: "Storage is busy. Try again." }),
      ),
    );
    const { user, dialog } = await openWithTwoRows();

    await choose(user, dialog, "Dataset", "faq");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    expect(await within(dialog).findByRole("alert")).toHaveTextContent(
      "Storage is busy. Try again.",
    );
  });
});

/** Opens the issue page of the fixture issue and its "Add examples to dataset" dialog. */
async function openIssueDialog() {
  server.use(
    http.get("/api/v1/projects/:project/issues/:id", () => HttpResponse.json(issueFixture)),
    http.get("/api/v1/projects/:project/issues/:id/occurrences", () =>
      HttpResponse.json(occurrencesFixture),
    ),
    http.get("/api/v1/projects/:project/issues/:id/events", () => HttpResponse.json(eventsFixture)),
  );
  const rendered = await renderApp(ISSUE_URL);
  await rendered.user.click(await screen.findByRole("button", { name: "Add examples to dataset" }));
  return { ...rendered, dialog: await screen.findByRole("dialog") };
}

describe("add issue examples to dataset dialog", () => {
  it("adds an issue's examples to the chosen dataset", async () => {
    const bodies: unknown[] = [];
    const issues: string[] = [];
    server.use(
      http.post(ISSUE_ITEMS, async ({ request, params }) => {
        bodies.push(await request.json());
        issues.push(String(params.id));
        return HttpResponse.json(fromTracesFixture, { status: 201 });
      }),
    );
    const { user, dialog } = await openIssueDialog();

    expect(
      within(dialog).getByRole("heading", { name: "Add examples to a dataset" }),
    ).toBeInTheDocument();
    await choose(user, dialog, "Dataset", "faq");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    expect(await within(dialog).findByText("Added 1 item to faq.")).toBeInTheDocument();
    expect(bodies).toEqual([{ dataset: FAQ_ID }]);
    expect(issues).toEqual([issueFixture.id]);
  });

  it("lists skipped traces from the issue route", async () => {
    server.use(http.post(ISSUE_ITEMS, () => HttpResponse.json(fromTracesFixture, { status: 201 })));
    const { user, dialog } = await openIssueDialog();

    await choose(user, dialog, "Dataset", "faq");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    expect(await within(dialog).findByText("1 trace was not copied")).toBeInTheDocument();
    expect(within(dialog).getByText("a1000000")).toBeInTheDocument();
    expect(within(dialog).getByText("the trace or span was not found")).toBeInTheDocument();
  });

  it("shows the 400 message when the issue has no occurrences", async () => {
    server.use(
      http.post(ISSUE_ITEMS, () =>
        problemResponse(400, "validation_failed", {
          errors: [{ field: "issue", message: "the issue has no occurrences in the data window" }],
        }),
      ),
    );
    const { user, dialog } = await openIssueDialog();

    await choose(user, dialog, "Dataset", "faq");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    expect(await within(dialog).findByRole("alert")).toHaveTextContent(
      "the issue has no occurrences in the data window",
    );
    expect(within(dialog).queryByRole("link", { name: "Open dataset" })).not.toBeInTheDocument();
  });

  it("creates a new dataset first", async () => {
    const order: string[] = [];
    server.use(
      http.post("/api/v1/projects/:project/datasets", async ({ request }) => {
        const body = (await request.json()) as { name: string };
        order.push(`create ${body.name}`);
        return HttpResponse.json(
          { ...datasetFixture, id: "01900000-0000-7000-8000-000000000c09", name: body.name },
          { status: 201 },
        );
      }),
      http.post(ISSUE_ITEMS, ({ params }) => {
        order.push(`items ${String(params.id)}`);
        return HttpResponse.json(fromTracesFixture, { status: 201 });
      }),
    );
    const { user, dialog } = await openIssueDialog();

    await choose(user, dialog, "Dataset", "New dataset…");
    await user.type(
      within(dialog).getByRole("textbox", { name: "Name of the new dataset" }),
      "golden",
    );
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    expect(await within(dialog).findByText("Added 1 item to golden.")).toBeInTheDocument();
    expect(order).toEqual(["create golden", `items ${issueFixture.id}`]);
  });
});
