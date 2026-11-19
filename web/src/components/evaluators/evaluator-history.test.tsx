import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { shortId } from "@/lib/format";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import versionsFixture from "../../../../fixtures/api/static/evaluator-versions.json";

const HISTORY_URL = "/acme/support-bot/evaluators/01900000-0000-7000-8000-000000000500?tab=history";
const FIXTURE_VERSION = versionsFixture.data[0];
if (FIXTURE_VERSION === undefined) {
  throw new Error("the versions fixture has no item");
}
const BASE = FIXTURE_VERSION;

/** One version of the fixture judge, with `changes` applied. */
function version(number: number, changes: Record<string, unknown> = {}) {
  return { ...BASE, version: number, ...changes };
}

/** One version with a prompt of `prompt` (the rest of the config as the fixture). */
function withPrompt(number: number, prompt: string) {
  return version(number, { config: { ...BASE.config, prompt } });
}

/** Answers the versions request with `pages`: the first request gets `pages[0]`, a cursor the next. */
function serveVersions(
  byCursor: Record<string, { data: unknown[]; next_cursor: string | null }>,
  first = "",
) {
  server.use(
    http.get("/api/v1/projects/:project/evaluators/:id/versions", ({ request }) => {
      const cursor = new URL(request.url).searchParams.get("cursor") ?? first;
      const page = byCursor[cursor];
      if (page === undefined) {
        return HttpResponse.json({ detail: "unexpected cursor" }, { status: 400 });
      }
      return HttpResponse.json(page);
    }),
  );
}

/** The text of every line of the page's diff with `op`. */
function diffLines(op: "added" | "removed" | "same") {
  return Array.from(document.querySelectorAll(`[data-op="${op}"]`)).map((line) => line.textContent);
}

it("shows the versions newest first", async () => {
  serveVersions({
    "": {
      data: [version(3), version(2), version(1)],
      next_cursor: null,
    },
  });
  await renderApp(HISTORY_URL);

  const buttons = await screen.findAllByRole("button", { name: /^v\d/ });
  expect(buttons.map((button) => button.textContent?.slice(0, 2))).toEqual(["v3", "v2", "v1"]);
  expect(buttons[0]).toHaveAttribute("aria-current", "true");
});

it("diffs the prompt against the previous version", async () => {
  serveVersions({
    "": {
      data: [withPrompt(3, "Line A\nNew line\nLine C"), withPrompt(2, "Line A\nOld line\nLine C")],
      next_cursor: null,
    },
  });
  await renderApp(HISTORY_URL);

  expect(await screen.findByText("Compared with version 2")).toBeInTheDocument();
  expect(diffLines("removed")).toEqual(["- Old line"]);
  expect(diffLines("added")).toEqual(["+ New line"]);
  expect(diffLines("same")).toEqual(["  Line A", "  Line C"]);
});

it("selecting an older version changes the diff", async () => {
  serveVersions({
    "": {
      data: [version(3, { name: "Relevance v3" }), version(2), version(1)],
      next_cursor: null,
    },
  });
  const { user } = await renderApp(HISTORY_URL);

  await user.click(await screen.findByRole("button", { name: /^v2/ }));
  expect(await screen.findByRole("heading", { name: "Version 2" })).toBeInTheDocument();
  expect(screen.getByText("Compared with version 1")).toBeInTheDocument();
});

it("lists changed config fields in a table", async () => {
  serveVersions({
    "": {
      data: [
        version(2, {
          config: { ...BASE.config, output: { type: "numeric", min: 0, max: 5 }, temperature: 0.2 },
        }),
        version(1),
      ],
      next_cursor: null,
    },
  });
  await renderApp(HISTORY_URL);

  const table = await screen.findByRole("table", { name: "Changed fields" });
  const rows = within(table).getAllByRole("row");
  const cells = rows.map((row) =>
    within(row)
      .queryAllByRole("cell")
      .map((cell) => cell.textContent),
  );
  expect(cells).toContainEqual(["output.max", "1", "5"]);
  expect(cells).toContainEqual(["temperature", "null", "0.2"]);
});

it("the first version shows everything as added", async () => {
  serveVersions({ "": { data: [version(1)], next_cursor: null } });
  await renderApp(HISTORY_URL);

  expect(await screen.findByText("First version")).toBeInTheDocument();
  expect(diffLines("removed")).toEqual([]);
  expect(diffLines("added").length).toBeGreaterThan(0);
});

it("an earliest recorded version above 1 is labelled so", async () => {
  serveVersions({
    "": {
      data: [withPrompt(4, "Line A\nNew\nLine C"), withPrompt(3, "Line A\nOld\nLine C")],
      next_cursor: null,
    },
  });
  const { user } = await renderApp(HISTORY_URL);
  expect(await screen.findByText("Compared with version 3")).toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: /^v3/ }));
  expect(await screen.findByText("Earliest recorded version")).toBeInTheDocument();
  expect(screen.queryByText("First version")).not.toBeInTheDocument();
  expect(diffLines("removed")).toEqual([]);
  expect(diffLines("added")).toContain("+ Old");
});

it("a missing previous page is fetched", async () => {
  serveVersions({
    "": { data: [version(3)], next_cursor: "c1" },
    c1: { data: [version(2), version(1)], next_cursor: null },
  });
  await renderApp(HISTORY_URL);

  expect(await screen.findByText("Compared with version 2")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Load more" })).not.toBeInTheDocument();
});

it("load more appends the next page", async () => {
  serveVersions({
    "": { data: [version(3), version(2)], next_cursor: "c1" },
    c1: { data: [version(1)], next_cursor: null },
  });
  const { user } = await renderApp(HISTORY_URL);

  await user.click(await screen.findByRole("button", { name: "Load more" }));
  await waitFor(() => expect(screen.getAllByRole("button", { name: /^v\d/ })).toHaveLength(3));
});

it("a gap is not papered over", async () => {
  serveVersions({ "": { data: [version(5), version(3)], next_cursor: null } });
  await renderApp(HISTORY_URL);

  expect(await screen.findByText("Earliest recorded version")).toBeInTheDocument();
  expect(screen.queryByText("Compared with version 3")).not.toBeInTheDocument();
});

it("webhook versions with no visible change say so", async () => {
  const hook = {
    ...BASE,
    kind: "webhook",
    config: { url_host: "hooks.example.com", timeout_ms: 5000, header_names: ["x-token"] },
  };
  serveVersions({
    "": {
      data: [
        { ...hook, version: 2 },
        { ...hook, version: 1 },
      ],
      next_cursor: null,
    },
  });
  await renderApp(HISTORY_URL);

  expect(await screen.findByText(/write-only/)).toBeInTheDocument();
});

it("shows the author as an id, and a dash when there is none", async () => {
  serveVersions({
    "": {
      data: [version(2), version(1, { created_by: undefined })],
      next_cursor: null,
    },
  });
  await renderApp(HISTORY_URL);

  expect(await screen.findByText(shortId(BASE.created_by ?? ""))).toBeInTheDocument();
  expect(screen.getByText("—")).toBeInTheDocument();
});

it("shows an empty history", async () => {
  serveVersions({ "": { data: [], next_cursor: null } });
  await renderApp(HISTORY_URL);

  expect(await screen.findByText("No version history yet")).toBeInTheDocument();
});
