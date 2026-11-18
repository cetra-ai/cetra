import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { afterEach, expect, it } from "vitest";
import { FiltersToggle } from "@/components/facets/facet-panel";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";

const TRACES = "/acme/support-bot/traces?range=15m";

const stops: (() => void)[] = [];

afterEach(() => {
  for (const stop of stops.splice(0)) {
    stop();
  }
});

/** Every facet request the page makes, as URLs. Stopped after the test. */
function recordFacets(): URL[] {
  const urls: URL[] = [];
  const listener = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/facets")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", listener);
  stops.push(() => server.events.removeListener("request:start", listener));
  return urls;
}

/** Answers the facets of one field with fixed values; every other field has none. */
function answerField(
  field: string,
  answer: { values?: { value: string; count: number }[]; missing?: number; kind?: string },
) {
  server.use(
    http.get("/api/v1/projects/:project/facets", ({ request }) => {
      const url = new URL(request.url);
      if (url.searchParams.get("field") !== field) {
        return HttpResponse.json({
          view: url.searchParams.get("view"),
          field: url.searchParams.get("field"),
          kind: "values",
          total: 0,
          missing: 0,
          values: [],
          more: false,
        });
      }
      const values = answer.values ?? [];
      return HttpResponse.json({
        view: url.searchParams.get("view"),
        field,
        kind: answer.kind ?? "values",
        total: values.reduce((sum, item) => sum + item.count, 0),
        missing: answer.missing ?? 0,
        values,
        more: false,
      });
    }),
  );
}

const STATUS = {
  values: [
    { value: "ok", count: 100 },
    { value: "error", count: 17 },
  ],
};

/** The section whose header matches `name`. Waits for the panel, which mounts with the list. */
async function section(name: RegExp): Promise<HTMLElement> {
  const header = await screen.findByRole("button", { name });
  const node = header.closest("section");
  if (!(node instanceof HTMLElement)) {
    throw new Error("facet section not found");
  }
  return node;
}

it("opens the default sections and keeps the others collapsed", async () => {
  await renderApp(TRACES);
  expect(await screen.findByRole("button", { name: /^Status/ })).toHaveAttribute(
    "aria-expanded",
    "true",
  );
  expect(screen.getByRole("button", { name: /^Models/ })).toHaveAttribute("aria-expanded", "false");
});

it("makes no facet request for a collapsed section until it is opened", async () => {
  const urls = recordFacets();
  const { user } = await renderApp(TRACES);
  await screen.findByRole("button", { name: /^Status/ });
  await waitFor(() => {
    expect(urls.some((url) => url.searchParams.get("field") === "status")).toBe(true);
  });
  expect(urls.some((url) => url.searchParams.get("field") === "models")).toBe(false);
  await user.click(screen.getByRole("button", { name: /^Models/ }));
  await waitFor(() => {
    expect(urls.some((url) => url.searchParams.get("field") === "models")).toBe(true);
  });
});

it("writes a checked value as an equality, and a second one as IN", async () => {
  answerField("status", STATUS);
  const { router, user } = await renderApp(TRACES);
  await user.click(await within(await section(/^Status/)).findByRole("checkbox", { name: "ok" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("status = 'ok'");
  });
  await user.click(
    await within(await section(/^Status/)).findByRole("checkbox", { name: "error" }),
  );
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("status IN ('ok', 'error')");
  });
});

it("excludes a value with the hover button", async () => {
  answerField("status", STATUS);
  const { router, user } = await renderApp(TRACES);
  await user.click(
    await within(await section(/^Status/)).findByRole("button", { name: "Exclude error" }),
  );
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("status != 'error'");
  });
});

it("writes the (none) row as IS NULL", async () => {
  answerField("status", { ...STATUS, missing: 3 });
  const { router, user } = await renderApp(TRACES);
  await user.click(
    await within(await section(/^Status/)).findByRole("checkbox", { name: "(none)" }),
  );
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("status IS NULL");
  });
});

it("counts a facet without its own conjuncts", async () => {
  answerField("status", STATUS);
  const urls = recordFacets();
  await renderApp(`${TRACES}&filter=${encodeURIComponent("status = 'ok' AND name = 'x'")}`);
  await waitFor(() => {
    const status = urls.find((url) => url.searchParams.get("field") === "status");
    expect(status?.searchParams.get("filter")).toBe("name = 'x'");
  });
});

it("filters the sections by their label", async () => {
  const { user } = await renderApp(TRACES);
  await user.type(await screen.findByRole("searchbox", { name: "Search filters" }), "stat");
  expect(screen.getByRole("button", { name: /^Status/ })).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: /^Environment/ })).not.toBeInTheDocument();
});

it("opens a metadata key into a nested values facet", async () => {
  answerField("metadata", {
    kind: "keys",
    values: [{ value: "team", count: 3 }],
  });
  const urls = recordFacets();
  const { user } = await renderApp(TRACES);
  await user.click(await screen.findByRole("button", { name: /^Metadata/ }));
  await user.click(await screen.findByRole("button", { name: /^team/ }));
  await waitFor(() => {
    expect(urls.some((url) => url.searchParams.get("field") === "metadata['team']")).toBe(true);
  });
});

it("hides the panel when the Filters button is pressed", async () => {
  const { user } = await renderApp(TRACES);
  const toggle = await screen.findByRole("button", { name: /^Filters/ });
  expect(toggle).toHaveAttribute("aria-pressed", "true");
  expect(await screen.findByRole("complementary", { name: "Filter facets" })).toBeInTheDocument();
  await user.click(toggle);
  expect(screen.queryByRole("complementary", { name: "Filter facets" })).not.toBeInTheDocument();
});

it("shows that counts are traces on the users list", async () => {
  await renderApp("/acme/support-bot/users?range=15m");
  expect(await screen.findByText("Counts are traces")).toBeInTheDocument();
});

it("resets the Text tab input when the section is cleared", async () => {
  const { router, user } = await renderApp(
    `${TRACES}&filter=${encodeURIComponent("name ILIKE '%foo%'")}`,
  );
  const text = await screen.findByRole("textbox", { name: "Name text" });
  expect(text).toHaveValue("foo");
  await user.click(within(await section(/^Name/)).getByRole("button", { name: "Clear" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBeUndefined();
  });
  // A filter change may remount the panel (its tab resets with it), so either way the text is empty.
  await waitFor(() => {
    expect(screen.queryByRole("textbox", { name: "Name text" })?.getAttribute("value") ?? "").toBe(
      "",
    );
  });
});

it("keeps the expanded sections across a reload", async () => {
  const { user } = await renderApp(TRACES);
  await user.click(await screen.findByRole("button", { name: /^Models/ }));
  expect(window.localStorage.getItem("cetra.facets.traces.expanded")).toContain("models");
  cleanup();
  await renderApp(TRACES);
  expect(await screen.findByRole("button", { name: /^Models/ })).toHaveAttribute(
    "aria-expanded",
    "true",
  );
});

it("keeps the panel closed across a reload", async () => {
  const { user } = await renderApp(TRACES);
  await user.click(await screen.findByRole("button", { name: /^Filters/ }));
  expect(window.localStorage.getItem("cetra.facets.open")).toBe("0");
  cleanup();
  await renderApp(TRACES);
  expect(await screen.findByRole("button", { name: /^Filters/ })).toHaveAttribute(
    "aria-pressed",
    "false",
  );
  expect(screen.queryByRole("complementary", { name: "Filter facets" })).not.toBeInTheDocument();
});

it("shows a selected value that the answer does not list, and unchecks it", async () => {
  answerField("status", { values: [{ value: "error", count: 17 }] });
  const { router, user } = await renderApp(
    `${TRACES}&filter=${encodeURIComponent("status = 'ok'")}`,
  );
  const ok = await within(await section(/^Status/)).findByRole("checkbox", { name: "ok" });
  expect(ok).toBeChecked();
  await user.click(ok);
  await waitFor(() => {
    expect(router.state.location.search.filter).toBeUndefined();
  });
});

it("badges a keys section and its owned keys, and clears them from the header", async () => {
  answerField("metadata", { kind: "keys", values: [{ value: "team", count: 3 }] });
  const { router, user } = await renderApp(
    `${TRACES}&filter=${encodeURIComponent("metadata['team'] = 'a'")}`,
  );
  await user.click(await screen.findByRole("button", { name: /^Metadata/ }));
  const team = await screen.findByRole("button", { name: /^team/ });
  expect(team).toHaveAttribute("aria-expanded", "true");
  expect(within(await section(/^Metadata/)).getAllByText("1").length).toBeGreaterThan(0);
  await user.click(within(await section(/^Metadata/)).getByRole("button", { name: "Clear" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBeUndefined();
  });
});

it("does not throw for an unterminated filter", () => {
  expect(() =>
    render(<FiltersToggle open filter="name = 'abc" onOpenChange={() => undefined} />),
  ).not.toThrow();
  expect(screen.getByRole("button", { name: /^Filters/ })).toBeInTheDocument();
});

it("names the toggle with its active count", () => {
  render(
    <FiltersToggle open filter="name = 'a' AND status = 'ok'" onOpenChange={() => undefined} />,
  );
  expect(screen.getByRole("button", { name: "Filters 2 active" })).toBeInTheDocument();
});
