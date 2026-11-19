import { expect, it } from "vitest";
import type { Dashboard } from "@/lib/dashboards/types";
import { dashboardHandlers, seedDashboards } from "@/mocks/dashboards";
import { server } from "@/mocks/node";
import dashboardDocument from "../../../fixtures/api/static/dashboard.json";
import overviewDocument from "../../../fixtures/api/static/dashboard-overview.json";
import staleDocument from "../../../fixtures/api/static/dashboard-stale.json";
import dashboardsDocument from "../../../fixtures/api/static/dashboards.json";

const LIST = new URL("/api/v1/projects/acme%2Fsupport-bot/dashboards", window.location.origin);
const ONE = `${LIST.href}/${dashboardDocument.id}`;
const OVERVIEW = `${LIST.href}/overview`;
const HEADING = { id: "h", title: "Costs", kind: "heading", layout: { x: 0, y: 0, w: 12, h: 1 } };

/** A stored dashboard with an empty definition, for a list of known names. */
function sample(id: string, name: string): Dashboard {
  return { ...dashboardDocument, id, name, definition: { widgets: [] }, version: 1 } as Dashboard;
}

function send(url: string, method: string, body?: unknown, query = "") {
  return fetch(`${url}${query}`, {
    method,
    headers: body === undefined ? {} : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

it("lists the seeded dashboards without definitions", async () => {
  const response = await send(LIST.href, "GET");
  expect(response.status).toBe(200);
  const page = await response.json();
  expect(page.next_cursor).toBeNull();
  expect(page.data).toEqual([expect.not.objectContaining({ definition: expect.anything() })]);
  expect(page.data[0]).toMatchObject({ id: dashboardsDocument.data[0]?.id, version: 3 });
});

it("pages the list with the cursor", async () => {
  server.use(
    ...dashboardHandlers(
      ["b", "a", "c"].map((name, index) => sample(`0190000000007000800000000000000${index}`, name)),
    ),
  );
  const first = await (await send(LIST.href, "GET", undefined, "?limit=2")).json();
  expect(first.data.map((item: { name: string }) => item.name)).toEqual(["a", "b"]);
  expect(first.next_cursor).toBe("2");
  const second = await (
    await send(LIST.href, "GET", undefined, `?limit=2&cursor=${first.next_cursor}`)
  ).json();
  expect(second.data.map((item: { name: string }) => item.name)).toEqual(["c"]);
  expect(second.next_cursor).toBeNull();
});

it("returns one dashboard with its definition, or 404", async () => {
  const found = await send(ONE, "GET");
  expect(await found.json()).toMatchObject({ definition: dashboardDocument.definition });
  const missing = await send(`${LIST.href}/01900000-0000-7000-8000-0000000000ff`, "GET");
  expect(missing.status).toBe(404);
});

it("creates a dashboard at version 1 and blanks its description to none", async () => {
  const response = await send(LIST.href, "POST", {
    name: "  Costs  ",
    description: "   ",
    definition: { widgets: [] },
  });
  expect(response.status).toBe(201);
  const created = await response.json();
  expect(created).toMatchObject({ name: "Costs", version: 1, definition: { widgets: [] } });
  expect(created).not.toHaveProperty("description");
});

it("answers 400 with the paths of each invalid field", async () => {
  const response = await send(LIST.href, "POST", {
    name: "",
    definition: {
      widgets: [{ id: "bad id", title: "x", kind: "heading", layout: { x: 0, y: 0, w: 1, h: 1 } }],
    },
  });
  expect(response.status).toBe(400);
  expect(await response.json()).toMatchObject({
    code: "bad_request",
    errors: [
      { field: "name", message: "must be 1 to 100 characters" },
      {
        field: "definition.widgets[0].id",
        message: "must be 1 to 64 characters of letters, digits, _ or -",
      },
    ],
  });
});

it("refuses the 101st dashboard with 409", async () => {
  const full = Array.from({ length: 100 }, (_, index) =>
    sample(`0190000000007000800000000000${String(index).padStart(4, "0")}`, "Full"),
  );
  server.use(...dashboardHandlers(full));
  const response = await send(LIST.href, "POST", { name: "One more", definition: { widgets: [] } });
  expect(response.status).toBe(409);
  expect(await response.json()).toMatchObject({ code: "conflict", detail: "too many dashboards" });
});

it("updates at the stored version and advances it by one", async () => {
  const response = await send(ONE, "PATCH", { version: 3, name: "Renamed", description: "" });
  expect(response.status).toBe(200);
  const updated = await response.json();
  expect(updated).toMatchObject({ name: "Renamed", version: 4 });
  expect(updated).not.toHaveProperty("description");
});

it("answers 409 with the stored version when the version is stale", async () => {
  const response = await send(ONE, "PATCH", { version: 2, name: "Late" });
  expect(response.status).toBe(409);
  expect(await response.json()).toEqual({
    ...staleDocument,
    detail: "the dashboard changed; current version is 3",
  });
});

it("answers 400 for an invalid definition and 404 for a missing dashboard", async () => {
  const invalid = await send(ONE, "PATCH", {
    version: 3,
    definition: {
      widgets: [{ id: "x", title: "", kind: "heading", layout: { x: 0, y: 0, w: 1, h: 1 } }],
    },
  });
  expect(invalid.status).toBe(400);
  const missing = await send(`${LIST.href}/01900000-0000-7000-8000-0000000000ff`, "PATCH", {
    version: 1,
    name: "x",
  });
  expect(missing.status).toBe(404);
});

it("deletes a dashboard once", async () => {
  const deleted = await send(ONE, "DELETE");
  expect(deleted.status).toBe(204);
  expect((await send(ONE, "DELETE")).status).toBe(404);
});

it("answers 404 for the Overview until one is saved", async () => {
  expect((await send(OVERVIEW, "GET")).status).toBe(404);
});

it("creates the Overview at version 1 on a save with no version, then updates it at the version read", async () => {
  const created = await send(OVERVIEW, "PUT", { definition: { widgets: [HEADING] } });
  expect(created.status).toBe(201);
  expect(await created.json()).toMatchObject({
    name: "Overview",
    version: 1,
    definition: { widgets: [HEADING] },
  });
  expect(await (await send(OVERVIEW, "GET")).json()).toMatchObject({ version: 1 });

  const updated = await send(OVERVIEW, "PUT", { version: 1, definition: { widgets: [] } });
  expect(updated.status).toBe(200);
  expect(await updated.json()).toMatchObject({ version: 2, definition: { widgets: [] } });
});

it("answers 409 with the stored version when the version read is stale", async () => {
  server.use(...dashboardHandlers(seedDashboards(), overviewDocument as unknown as Dashboard));
  const response = await send(OVERVIEW, "PUT", { version: 2, definition: { widgets: [] } });
  expect(response.status).toBe(409);
  expect(await response.json()).toEqual({
    ...staleDocument,
    detail: "the dashboard changed; current version is 3",
  });
});

it("answers 409 for a save without a version when one is stored", async () => {
  server.use(...dashboardHandlers(seedDashboards(), overviewDocument as unknown as Dashboard));
  const response = await send(OVERVIEW, "PUT", { definition: { widgets: [] } });
  expect(response.status).toBe(409);
  expect(await response.json()).toMatchObject({
    detail: "the dashboard changed; current version is 3",
  });
});

it("answers 409 with 0 for a save with a version when none is stored", async () => {
  const response = await send(OVERVIEW, "PUT", { version: 1, definition: { widgets: [] } });
  expect(response.status).toBe(409);
  expect(await response.json()).toMatchObject({
    detail: "the dashboard changed; current version is 0",
  });
});

it("answers 400 with the paths of an invalid Overview", async () => {
  const response = await send(OVERVIEW, "PUT", {
    definition: {
      widgets: [{ id: "bad id", title: "x", kind: "heading", layout: { x: 0, y: 0, w: 1, h: 1 } }],
    },
  });
  expect(response.status).toBe(400);
  expect(await response.json()).toMatchObject({
    code: "bad_request",
    errors: [
      {
        field: "definition.widgets[0].id",
        message: "must be 1 to 64 characters of letters, digits, _ or -",
      },
    ],
  });
});

it("serves a seeded Overview, and deletes it once it is reset", async () => {
  server.use(...dashboardHandlers(seedDashboards(), overviewDocument as unknown as Dashboard));
  expect(await (await send(OVERVIEW, "GET")).json()).toMatchObject({
    name: "Overview",
    version: 3,
  });
  expect((await send(OVERVIEW, "DELETE")).status).toBe(204);
  expect((await send(OVERVIEW, "GET")).status).toBe(404);
});
