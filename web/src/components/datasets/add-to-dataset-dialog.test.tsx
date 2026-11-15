import { screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { payloadJson, prettyJson } from "@/lib/datasets";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import datasetFixture from "../../../../fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01.json";
import rootSpanFixture from "../../../../fixtures/api/responses/spans/a1000000000000000000000000000001/b100000000000011.json";
import fromTraceFixture from "../../../../fixtures/api/static/dataset-item-from-trace.json";
import projectFixture from "../../../../fixtures/api/static/project.json";

const TRACE = "a1000000000000000000000000000001";
const ROOT = "b100000000000011";
const SPAN = "b100000000000014";
const FROM_TRACE = "/api/v1/projects/:project/datasets/:dataset/items/from-trace";
const NOT_FOUND =
  "The trace was not found. It may have been deleted or be outside your plan's data window.";

/** Answers each from-trace request with 201 and records the request. */
function captureItems(): { bodies: unknown[]; datasets: string[] } {
  const bodies: unknown[] = [];
  const datasets: string[] = [];
  server.use(
    http.post(FROM_TRACE, async ({ request, params }) => {
      bodies.push(await request.json());
      datasets.push(String(params.dataset));
      return HttpResponse.json(fromTraceFixture, { status: 201 });
    }),
  );
  return { bodies, datasets };
}

/** Opens the trace view of `url` and its "Add to dataset" dialog. */
async function openDialog(url: string) {
  const rendered = await renderApp(url);
  const header = await screen.findByRole("region", { name: "Trace" });
  await rendered.user.click(within(header).getByRole("button", { name: "Add to dataset" }));
  return { ...rendered, dialog: await screen.findByRole("dialog") };
}

/** Picks an option of the Radix select named `name`, inside the dialog. */
async function choose(user: UserEvent, dialog: HTMLElement, name: string, option: string) {
  await user.click(within(dialog).getByRole("combobox", { name }));
  await user.click(await screen.findByRole("option", { name: option }));
}

/** Replaces the text of a textarea. Pasting keeps `{` and `[` literal for user-event. */
async function setText(user: UserEvent, dialog: HTMLElement, label: string, text: string) {
  const field = within(dialog).getByRole("textbox", { name: label });
  await user.click(field);
  await user.paste(text);
}

const TRACE_URL = `/acme/support-bot/traces?mock=fixtures&trace=${TRACE}`;
const SPAN_URL = `/acme/support-bot/traces?mock=fixtures&trace=${TRACE}&span=${SPAN}`;

describe("add to dataset dialog", () => {
  it("prefills from the root span and adds the item", async () => {
    const { bodies } = captureItems();
    const { user, dialog } = await openDialog(TRACE_URL);

    const rootInput = prettyJson(payloadJson(rootSpanFixture.input as Schema<"Payload">));
    await waitFor(() =>
      expect(within(dialog).getByRole("textbox", { name: "Input" })).toHaveValue(rootInput),
    );
    await choose(user, dialog, "Dataset", "faq");
    await user.click(within(dialog).getByRole("button", { name: "Add item" }));

    expect(await within(dialog).findByText("Added to faq.")).toBeInTheDocument();
    expect(bodies).toHaveLength(1);
    expect(bodies[0]).toMatchObject({ trace_id: TRACE, span_id: ROOT });
  });

  it("adds the selected span", async () => {
    const { bodies } = captureItems();
    const { user, dialog } = await openDialog(SPAN_URL);

    await user.click(within(dialog).getByRole("radio", { name: "Selected span" }));
    expect(within(dialog).getByRole("radio", { name: "Selected span" })).toBeChecked();
    await choose(user, dialog, "Dataset", "faq");
    await user.click(within(dialog).getByRole("button", { name: "Add item" }));

    await waitFor(() => expect(bodies).toHaveLength(1));
    expect(bodies[0]).toMatchObject({ trace_id: TRACE, span_id: SPAN });
  });

  it("creates a dataset before adding the item", async () => {
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
    );
    server.use(
      http.post(FROM_TRACE, async ({ params }) => {
        order.push(`item ${String(params.dataset)}`);
        return HttpResponse.json(fromTraceFixture, { status: 201 });
      }),
    );
    const { user, dialog } = await openDialog(TRACE_URL);

    await choose(user, dialog, "Dataset", "New dataset…");
    await user.type(
      within(dialog).getByRole("textbox", { name: "Name of the new dataset" }),
      "golden",
    );
    await user.click(within(dialog).getByRole("button", { name: "Add item" }));

    expect(await within(dialog).findByText("Added to golden.")).toBeInTheDocument();
    expect(order).toEqual(["create golden", "item 01900000-0000-7000-8000-000000000c09"]);
  });

  it("keeps edited fields when the source changes", async () => {
    captureItems();
    const { user, dialog } = await openDialog(SPAN_URL);

    const expected = within(dialog).getByRole("textbox", { name: "Expected output" });
    await waitFor(() => expect(expected).toBeEnabled());
    await user.clear(expected);
    await setText(user, dialog, "Expected output", '"typed by hand"');
    await user.click(within(dialog).getByRole("radio", { name: "Trace" }));

    await waitFor(() =>
      expect(within(dialog).getByRole("textbox", { name: "Expected output" })).toHaveValue(
        '"typed by hand"',
      ),
    );
  });

  it("searches the dataset picker", async () => {
    const queries: (string | null)[] = [];
    server.use(
      http.get("/api/v1/projects/:project/datasets", ({ request }) => {
        queries.push(new URL(request.url).searchParams.get("q"));
        return HttpResponse.json({ data: [], next_cursor: null });
      }),
    );
    const { user, dialog } = await openDialog(TRACE_URL);

    await user.type(within(dialog).getByRole("textbox", { name: "Search datasets" }), "gold");

    await waitFor(() => expect(queries).toContain("gold"));
  });

  it("explains a missing trace", async () => {
    server.use(
      http.post(FROM_TRACE, () => problemResponse(404, "not_found", { title: "Not found" })),
    );
    const { user, dialog } = await openDialog(TRACE_URL);

    await choose(user, dialog, "Dataset", "faq");
    await user.click(within(dialog).getByRole("button", { name: "Add item" }));

    expect(await within(dialog).findByText(NOT_FOUND)).toBeInTheDocument();
  });

  it("is disabled for viewers", async () => {
    server.use(
      http.get("/api/v1/projects/:project", () =>
        HttpResponse.json({ ...projectFixture, role: "viewer" }),
      ),
    );
    await renderApp(TRACE_URL);
    const header = await screen.findByRole("region", { name: "Trace" });

    expect(within(header).getByRole("button", { name: "Add to dataset" })).toBeDisabled();
  });
});
