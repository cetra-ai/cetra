import { screen, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import backfillsFixture from "../../../../fixtures/api/static/backfills.json";
import { backfillRefetchInterval } from "./backfills-table";

const RULE_URL = "/acme/support-bot/online-rules/01900000-0000-7000-8000-000000000700";
const LIST = "/api/v1/projects/:project/online-rules/:id/backfills";

it("lists backfills with their status, counts and error", async () => {
  await renderApp(RULE_URL);

  const table = await screen.findByRole("table", { name: "Backfills" });
  // Row 0 is the header.
  const rows = within(table).getAllByRole("row");
  const done = rows[1] as HTMLElement;
  const failed = rows[2] as HTMLElement;
  expect(within(done).getByText("Done")).toBeInTheDocument();
  expect(within(done).getByText("1,000")).toBeInTheDocument();
  expect(within(done).getByText("240")).toBeInTheDocument();
  expect(within(failed).getByText("Failed")).toBeInTheDocument();
  expect(
    within(failed).getByText("Interrupted by a server restart. Run it again."),
  ).toBeInTheDocument();
});

it("says when there are no backfills", async () => {
  server.use(http.get(LIST, () => HttpResponse.json({ data: [], next_cursor: null })));
  await renderApp(RULE_URL);

  expect(await screen.findByText("No backfills yet.")).toBeInTheDocument();
});

it("says when the list cannot be loaded", async () => {
  server.use(http.get(LIST, () => HttpResponse.json({}, { status: 500 })));
  await renderApp(RULE_URL);

  expect(await screen.findByRole("alert")).toHaveTextContent("The backfills could not be loaded.");
});

it("refetches only while a backfill runs", () => {
  const page = backfillsFixture as Schema<"BackfillPage">;
  const running = {
    ...page.data[0],
    status: "running",
    finished_at: undefined,
  } as Schema<"Backfill">;

  expect(backfillRefetchInterval(page)).toBe(false);
  expect(backfillRefetchInterval({ data: [running], next_cursor: null })).toBe(2000);
  expect(backfillRefetchInterval(undefined)).toBe(false);
});
