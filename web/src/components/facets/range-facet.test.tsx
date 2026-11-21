import { screen, waitFor, within } from "@testing-library/react";
import { expect, it } from "vitest";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";
import { facetsHandler } from "@/test/facets";
import { renderApp } from "@/test/render";

const TRACES = "/acme/support-bot/traces?range=15m";

const HISTOGRAM = {
  min: 1,
  max: 9000,
  p50: 120,
  p95: 2400,
  p99: 7000,
  buckets: [
    { lo: 1, hi: 351.4, count: 80 },
    { lo: 351.4, hi: 701.8, count: 20 },
    { lo: 7000, hi: 9000, count: 5 },
  ],
};

/** Answers the facet of one field with a histogram (or none); other fields have no values. */
function answerHistogram(field: string, histogram?: Schema<"FacetHistogram">) {
  server.use(
    facetsHandler((ask): Schema<"Facet"> => {
      if (ask.field !== field) {
        return { kind: "values", total: 100, missing: 0, values: [], more: false };
      }
      return {
        kind: "histogram",
        total: 100,
        missing: 0,
        ...(histogram !== undefined ? { histogram } : {}),
      };
    }),
  );
}

/** The Duration section, which the traces list opens by default. */
async function durationSection(): Promise<HTMLElement> {
  const panel = await screen.findByRole("complementary", { name: "Filter facets" });
  const header = await within(panel).findByRole("button", { name: /^Duration/ });
  const node = header.closest("section");
  if (!(node instanceof HTMLElement)) {
    throw new Error("facet section not found");
  }
  return node;
}

/** The bars of the Duration section, by their label's count suffix. */
async function bar(count: number): Promise<HTMLElement> {
  return within(await durationSection()).findByRole("button", {
    name: new RegExp(`: ${count}$`),
  });
}

it("draws a bar per bucket and the percentiles", async () => {
  answerHistogram("duration_ms", HISTOGRAM);
  await renderApp(TRACES);
  expect(await bar(80)).toBeInTheDocument();
  expect(within(await durationSection()).getAllByRole("button", { name: /: \d+$/ })).toHaveLength(
    3,
  );
  expect(within(await durationSection()).getByText(/p95/)).toBeInTheDocument();
});

it("writes the range of a clicked bar", async () => {
  answerHistogram("duration_ms", HISTOGRAM);
  const { router, user } = await renderApp(TRACES);
  await user.click(await bar(80));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("duration_ms >= 1 AND duration_ms < 351.4");
  });
});

it("writes only a lower bound for the last bar, which is closed at max", async () => {
  answerHistogram("duration_ms", HISTOGRAM);
  const { router, user } = await renderApp(TRACES);
  await user.click(await bar(5));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("duration_ms >= 7000");
  });
});

it("closes an open upper bound at the shift-clicked bar", async () => {
  answerHistogram("duration_ms", HISTOGRAM);
  const filter = encodeURIComponent("duration_ms >= 351.4");
  const { router, user } = await renderApp(`${TRACES}&filter=${filter}`);
  await user.keyboard("{Shift>}");
  await user.click(await bar(80));
  await user.keyboard("{/Shift}");
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("duration_ms >= 1 AND duration_ms < 351.4");
  });
});

it("keeps an open upper bound open when the shift-clicked bar is the last", async () => {
  answerHistogram("duration_ms", HISTOGRAM);
  const filter = encodeURIComponent("duration_ms >= 351.4");
  const { router, user } = await renderApp(`${TRACES}&filter=${filter}`);
  await user.keyboard("{Shift>}");
  await user.click(await bar(5));
  await user.keyboard("{/Shift}");
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("duration_ms >= 351.4");
  });
});

it("extends the selection to a shift-clicked bar", async () => {
  answerHistogram("duration_ms", HISTOGRAM);
  const filter = encodeURIComponent("duration_ms >= 351.4 AND duration_ms < 701.8");
  const { router, user } = await renderApp(`${TRACES}&filter=${filter}`);
  await user.keyboard("{Shift>}");
  await user.click(await bar(80));
  await user.keyboard("{/Shift}");
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("duration_ms >= 1 AND duration_ms < 701.8");
  });
});

it("writes the typed bounds, and leaves an empty bound unbounded", async () => {
  answerHistogram("duration_ms", HISTOGRAM);
  const { router, user } = await renderApp(TRACES);
  const section = await durationSection();
  await user.type(await within(section).findByRole("textbox", { name: "Duration min" }), "10");
  await user.type(await within(section).findByRole("textbox", { name: "Duration max" }), "20");
  await user.click(await within(section).findByRole("button", { name: "Apply" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("duration_ms >= 10 AND duration_ms < 20");
  });
});

it("disables Apply for a bound too small to write, or not finite", async () => {
  answerHistogram("duration_ms", HISTOGRAM);
  const { user } = await renderApp(TRACES);
  const section = await durationSection();
  const min = await within(section).findByRole("textbox", { name: "Duration min" });
  await user.type(min, "1e-12");
  expect(await within(section).findByRole("button", { name: "Apply" })).toBeDisabled();
  expect(
    await within(section).findByText("Too small; use 0 or a larger value"),
  ).toBeInTheDocument();
  await user.clear(min);
  await user.type(
    await within(section).findByRole("textbox", { name: "Duration max" }),
    "Infinity",
  );
  expect(await within(section).findByRole("button", { name: "Apply" })).toBeDisabled();
});

it("disables Apply and says so when min is not below max", async () => {
  answerHistogram("duration_ms", HISTOGRAM);
  const { user } = await renderApp(TRACES);
  const section = await durationSection();
  await user.type(await within(section).findByRole("textbox", { name: "Duration min" }), "20");
  await user.type(await within(section).findByRole("textbox", { name: "Duration max" }), "10");
  expect(await within(section).findByRole("button", { name: "Apply" })).toBeDisabled();
  expect(await within(section).findByText("Min must be below max")).toBeInTheDocument();
});

it("disables the bars and inputs for a range it cannot edit", async () => {
  answerHistogram("duration_ms", HISTOGRAM);
  const filter = encodeURIComponent("duration_ms > 5");
  await renderApp(`${TRACES}&filter=${filter}`);
  const section = await durationSection();
  expect(await within(section).findByText("Edited as text")).toBeInTheDocument();
  expect(await bar(80)).toBeDisabled();
  expect(await within(section).findByRole("textbox", { name: "Duration min" })).toBeDisabled();
  expect(await within(section).findByRole("button", { name: "Apply" })).toBeDisabled();
});

it("shows No values when the field has no histogram", async () => {
  answerHistogram("duration_ms");
  await renderApp(TRACES);
  const section = await durationSection();
  await waitFor(() => {
    expect(within(section).getByText("No values")).toBeInTheDocument();
  });
});
