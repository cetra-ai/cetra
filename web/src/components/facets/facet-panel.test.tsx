import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import { delay, HttpResponse, http } from "msw";
import { afterEach, expect, it, vi } from "vitest";
import { FACET_BATCH_MS } from "@/api/facet-batch";
import type { Schema } from "@/api/types";
import { FiltersToggle } from "@/components/facets/facet-panel";
import { ANCHOR_REUSE_MS } from "@/lib/list-anchor";
import { server } from "@/mocks/node";
import { facetsHandler } from "@/test/facets";
import { renderApp } from "@/test/render";

const TRACES = "/acme/support-bot/traces?range=15m";

const stops: (() => void)[] = [];

afterEach(() => {
  for (const stop of stops.splice(0)) {
    stop();
  }
});

/** Every facet request the page makes, as parsed bodies. Stopped after the test. */
function recordFacets(): Schema<"FacetsRequest">[] {
  const bodies: Schema<"FacetsRequest">[] = [];
  const listener = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (request.method === "POST" && url.pathname.endsWith("/facets")) {
      void request
        .clone()
        .json()
        .then((body) => {
          bodies.push(body as Schema<"FacetsRequest">);
        });
    }
  };
  server.events.on("request:start", listener);
  stops.push(() => server.events.removeListener("request:start", listener));
  return bodies;
}

/** The fields the page asked, in the order their requests went out. */
function fieldsAsked(bodies: Schema<"FacetsRequest">[]): string[] {
  return bodies.flatMap((body) => body.fields.map((field) => field.field));
}

/** Answers the facets of one field with fixed values; every other field has none. */
function answerField(
  field: string,
  answer: {
    values?: { value: string; count: number }[];
    missing?: number;
    kind?: "values" | "keys";
  },
) {
  server.use(
    facetsHandler((ask) => {
      if (ask.field !== field) {
        return undefined;
      }
      const values = answer.values ?? [];
      return {
        kind: answer.kind ?? "values",
        total: values.reduce((sum, item) => sum + item.count, 0),
        missing: answer.missing ?? 0,
        values,
        more: false,
      };
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
  const bodies = recordFacets();
  const { user } = await renderApp(TRACES);
  await screen.findByRole("button", { name: /^Status/ });
  await waitFor(() => {
    expect(fieldsAsked(bodies)).toContain("status");
  });
  expect(fieldsAsked(bodies)).not.toContain("models");
  await user.click(screen.getByRole("button", { name: /^Models/ }));
  await waitFor(() => {
    expect(fieldsAsked(bodies)).toContain("models");
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
  const bodies = recordFacets();
  await renderApp(`${TRACES}&filter=${encodeURIComponent("status = 'ok' AND name = 'x'")}`);
  await waitFor(() => {
    const status = bodies.find((body) => body.fields.some((field) => field.field === "status"));
    expect(status?.filter).toBe("name = 'x'");
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
  const bodies = recordFacets();
  const { user } = await renderApp(TRACES);
  await user.click(await screen.findByRole("button", { name: /^Metadata/ }));
  await user.click(await screen.findByRole("button", { name: /^team/ }));
  await waitFor(() => {
    expect(fieldsAsked(bodies)).toContain("metadata['team']");
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

it("opening the spans page sends one facet request, and opening a section sends one more", async () => {
  // Batches an earlier test asked for go out within FACET_BATCH_MS; let them leave first.
  await new Promise((resolve) => setTimeout(resolve, FACET_BATCH_MS * 3));
  const bodies = recordFacets();
  const { user } = await renderApp("/acme/support-bot/spans?range=15m");
  await section(/^Status/);
  // The open sections (span kind, status, name, duration) and the Slow and Expensive presets (their
  // p95s: duration and cost) mount together, so one batch asks them all.
  await waitFor(() => {
    expect(bodies).toHaveLength(1);
  });
  // No straggler follows: a section or chip mounting late would send a second batch.
  await new Promise((resolve) => setTimeout(resolve, FACET_BATCH_MS * 5));
  expect(bodies).toHaveLength(1);
  expect(fieldsAsked(bodies)).toEqual(
    expect.arrayContaining(["cost_total", "duration_ms", "name", "span_kind", "status"]),
  );

  await user.click(await screen.findByRole("button", { name: /^Model/ }));
  await waitFor(() => {
    expect(bodies).toHaveLength(2);
  });
  expect(bodies[1]?.fields.map((field) => field.field)).toEqual(["model"]);
});

it("shows the last counts at once when coming back to the list", async () => {
  answerField("status", STATUS);
  const bodies = recordFacets();
  const { router } = await renderApp(TRACES);
  await within(await section(/^Status/)).findByRole("checkbox", { name: "ok" });
  const firstFrom = bodies.find((body) =>
    body.fields.some((field) => field.field === "status"),
  )?.from;

  await router.navigate({
    to: "/$org/$project/spans",
    params: { org: "acme", project: "support-bot" },
  });
  await screen.findByRole("button", { name: /^Status/ });
  // A list keeps its window for 15 minutes (D-C-23); past that, coming back asks for a later
  // window that is not cached.
  vi.useFakeTimers({ toFake: ["Date"] });
  vi.setSystemTime(Date.now() + ANCHOR_REUSE_MS + 20_000);
  server.use(
    http.post("/api/v1/projects/:project/facets", async () => {
      await delay("infinite");
      return HttpResponse.json({});
    }),
  );
  try {
    await router.navigate({
      to: "/$org/$project/traces",
      params: { org: "acme", project: "support-bot" },
      search: { range: "15m" },
    });
    const status = await section(/^Status/);
    expect(within(status).getByRole("checkbox", { name: "ok" })).toBeInTheDocument();
    expect(status.querySelector('[data-slot="skeleton"]')).toBeNull();
    await waitFor(() => {
      const asked = bodies.filter(
        (body) => body.view === "traces" && body.fields.some((field) => field.field === "status"),
      );
      expect(asked.at(-1)?.from).not.toBe(firstFrom);
    });
  } finally {
    vi.useRealTimers();
  }
});
