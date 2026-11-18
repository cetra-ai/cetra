import { afterEach, describe, expect, it, vi } from "vitest";
import { STORAGE_PREFIX } from "@/product";
import { localStore } from "./store";
import type { Definition } from "./types";

const EMPTY: Definition = { widgets: [] };
const KEY = `${STORAGE_PREFIX}dashboards.acme/store-tests`;

afterEach(() => {
  vi.restoreAllMocks();
  localStorage.clear();
});

describe("localStore", () => {
  it("creates, lists, reads, updates and deletes a dashboard", async () => {
    const store = localStore("acme", "store-tests");
    const created = await store.create({
      name: "  Costs  ",
      description: " by model ",
      definition: EMPTY,
    });
    expect(created).toMatchObject({ name: "Costs", description: "by model", version: 1 });
    expect(created.id).toMatch(/^[0-9a-f-]{36}$/);

    expect(await store.list()).toEqual([
      expect.objectContaining({ id: created.id, name: "Costs" }),
    ]);
    expect(await store.get(created.id)).toEqual(created);

    const renamed = await store.update(created.id, { version: 1, name: "Spend", description: "" });
    expect(renamed).toMatchObject({ name: "Spend", version: 2 });
    expect(renamed.description).toBeUndefined();
    expect(renamed.definition).toEqual(EMPTY);

    await store.remove(created.id);
    expect(await store.list()).toEqual([]);
    await expect(store.get(created.id)).rejects.toMatchObject({ status: 404, code: "not_found" });
    await expect(store.remove(created.id)).rejects.toMatchObject({ status: 404 });
  });

  it("increments the version on every update", async () => {
    const store = localStore("acme", "store-tests");
    const { id } = await store.create({ name: "Costs", definition: EMPTY });
    const once = await store.update(id, { version: 1, name: "Costs 2" });
    const twice = await store.update(id, { version: 2, definition: { widgets: [] } });
    expect([once.version, twice.version]).toEqual([2, 3]);
  });

  it("answers the server's validation errors with paths", async () => {
    const store = localStore("acme", "store-tests");
    await expect(store.create({ name: "  ", definition: EMPTY })).rejects.toMatchObject({
      status: 400,
      code: "bad_request",
      errors: [{ field: "name", message: "must be 1 to 100 characters" }],
    });
    await expect(
      store.create({
        name: "Costs",
        definition: {
          widgets: [
            {
              id: "a",
              title: "Hi",
              kind: "heading",
              layout: { x: 0, y: 0, w: 61, h: 1 },
            },
          ],
        },
      }),
    ).rejects.toMatchObject({
      status: 400,
      errors: [{ field: "definition.widgets[0].layout.w", message: "must be 1 to 60" }],
    });
    expect(await store.list()).toEqual([]);
  });

  it("refuses the 101st dashboard with the server's conflict", async () => {
    const store = localStore("acme", "store-tests");
    for (let at = 0; at < 100; at += 1) {
      await store.create({ name: `Dashboard ${at}`, definition: EMPTY });
    }
    await expect(store.create({ name: "One too many", definition: EMPTY })).rejects.toMatchObject({
      status: 409,
      code: "conflict",
      detail: "too many dashboards",
    });
    expect(await store.list()).toHaveLength(100);
  });

  it("rejects a write when storage throws, and stores nothing", async () => {
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("quota");
    });
    const store = localStore("acme", "store-tests");
    await expect(store.create({ name: "Lost", definition: EMPTY })).rejects.toMatchObject({
      status: 507,
      code: "internal",
      detail: "Could not save in this browser: its storage is full or unavailable.",
    });
    expect(localStorage.getItem(KEY)).toBeNull();
    expect(await store.list()).toEqual([]);
  });

  it("an unavailable localStorage reads as empty and refuses writes", async () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    const store = localStore("acme", "store-tests");
    expect(await store.list()).toEqual([]);
    await expect(store.get("missing")).rejects.toMatchObject({ status: 404 });
    await expect(store.create({ name: "Refused", definition: EMPTY })).rejects.toMatchObject({
      status: 409,
      code: "conflict",
      detail: "Saved dashboards in this browser cannot be read; nothing was changed.",
    });
  });

  it("refuses to write over unreadable saved dashboards", async () => {
    const store = localStore("acme", "store-tests");
    const detail = "Saved dashboards in this browser cannot be read; nothing was changed.";
    for (const text of ["{not json", JSON.stringify({ widgets: [] })]) {
      localStorage.setItem(KEY, text);
      expect(await store.list()).toEqual([]);
      await expect(store.create({ name: "Nope", definition: EMPTY })).rejects.toMatchObject({
        status: 409,
        code: "conflict",
        detail,
      });
      await expect(store.update("any", { version: 1, name: "Nope" })).rejects.toMatchObject({
        status: 409,
        detail,
      });
      await expect(store.remove("any")).rejects.toMatchObject({ status: 409, detail });
      expect(localStorage.getItem(KEY)).toBe(text);
    }
  });

  it("skips an invalid record and keeps it on the next write", async () => {
    const store = localStore("acme", "store-tests");
    const { id } = await store.create({ name: "Kept", definition: EMPTY });
    const stored = JSON.parse(localStorage.getItem(KEY) ?? "[]") as unknown[];
    const invalid = { id: "bad", name: 3, definition: {} };
    localStorage.setItem(KEY, JSON.stringify([invalid, ...stored]));

    expect(await store.list()).toEqual([expect.objectContaining({ id })]);
    await store.create({ name: "Second", definition: EMPTY });

    const after = JSON.parse(localStorage.getItem(KEY) ?? "[]") as unknown[];
    expect(after).toHaveLength(3);
    expect(after[0]).toEqual(invalid);
    expect(await store.list()).toHaveLength(2);
  });

  it("update with an empty description clears it", async () => {
    const store = localStore("acme", "store-tests");
    const { id } = await store.create({
      name: "Costs",
      description: "by model",
      definition: EMPTY,
    });
    const cleared = await store.update(id, { version: 1, description: "   " });
    expect(cleared).not.toHaveProperty("description");
    expect(await store.get(id)).not.toHaveProperty("description");
  });

  it("has no Overview: it reads as the preset, and refuses a save or a reset", async () => {
    const store = localStore("acme", "store-tests");
    expect(await store.getOverview()).toBeNull();
    await expect(store.saveOverview({ definition: EMPTY })).rejects.toMatchObject({
      status: 403,
      code: "forbidden",
      detail: "The Overview cannot be changed here.",
    });
    await expect(store.resetOverview()).rejects.toMatchObject({ status: 403, code: "forbidden" });
    expect(localStorage.getItem(KEY)).toBeNull();
  });

  it("reads a value that does not parse, or has the wrong shape, as an empty list", async () => {
    const store = localStore("acme", "store-tests");
    localStorage.setItem(KEY, "{not json");
    expect(await store.list()).toEqual([]);

    localStorage.setItem(KEY, JSON.stringify({ widgets: [] }));
    expect(await store.list()).toEqual([]);

    localStorage.setItem(KEY, JSON.stringify([{ id: "x", name: 3 }]));
    expect(await store.list()).toEqual([]);

    localStorage.setItem(KEY, JSON.stringify([null]));
    expect(await store.list()).toEqual([]);

    const created = await store.create({ name: "Fresh", definition: EMPTY });
    expect(await store.get(created.id)).toEqual(created);
  });
});
