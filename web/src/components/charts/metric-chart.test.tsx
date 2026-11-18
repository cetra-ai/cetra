import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { DashboardBody, DashboardBodySkeleton } from "@/components/charts/metric-chart";
import { xAxisTicks } from "@/components/charts/plot-helpers";
import type { ChartRow } from "@/lib/metrics";

const TITLES = [
  "Traces & errors",
  "Error rate",
  "Latency",
  "LLM latency by model",
  "Cost by model",
  "Tokens by type",
  "TTFT",
  "Scores",
  "Evaluation cost",
  "Top models by cost",
];

it("shows every chart card as a busy region with its title", () => {
  render(<DashboardBodySkeleton />);
  expect(screen.getAllByRole("region")).toHaveLength(TITLES.length);
  for (const title of TITLES) {
    const region = screen.getByRole("region", { name: title });
    expect(region).toHaveAttribute("aria-busy", "true");
    expect(within(region).getByRole("heading", { name: title })).toBeInTheDocument();
  }
});

const HOURS = [0, 1, 2].map((hour) => `2026-11-17T0${hour}:00:00Z`);

function points(values: readonly (number | null)[]) {
  return HOURS.map((time, index) => {
    const value = values[index];
    return value === null || value === undefined ? { time } : { time, value };
  });
}

/** Three hourly buckets with errors, a 0.16 peak error rate, latency up to 2.5 s and two models. */
const RESPONSE: Schema<"MetricsResponse"> = {
  bucket_ns: 3_600_000_000_000,
  from: HOURS[0] ?? "",
  to: "2026-11-17T03:00:00Z",
  kpis: [],
  top_models: [
    { model: "claude-3", llm_count: 4, cost_total: 3 },
    { model: "gpt-4o", llm_count: 2, cost_total: 1 },
  ],
  series: [
    { metric: "traces", points: points([10, 12, 8]) },
    { metric: "errors", points: points([2, 0, 1]) },
    { metric: "error_rate", points: points([0.16, 0, 0.04]) },
    { metric: "latency_p50", points: points([1_000_000_000, 2_000_000_000, 2_500_000_000]) },
    { metric: "latency_p95", points: points([2_000_000_000, 2_500_000_000, 1_000_000_000]) },
    { metric: "cost_by_model", group: "gpt-4o", points: points([0.5, 0.25, 0.25]) },
    { metric: "cost_by_model", group: "claude-3", points: points([1, 1, 1]) },
    { metric: "cost_by_model", group: "gemini", points: points([0.1, 0.1, 0.1]) },
    { metric: "llm_latency_p50", group: "gpt-4o", points: points([1e9, 1e9, 1e9]) },
    { metric: "llm_latency_p50", group: "claude-3", points: points([2e9, 2e9, 2e9]) },
  ],
};

function renderBody() {
  return render(<DashboardBody response={RESPONSE} onBucket={() => {}} />);
}

function card(title: string): HTMLElement {
  return screen.getByRole("region", { name: new RegExp(`^${title.replace("&", "\\&")}: `) });
}

function strokes(region: HTMLElement): string[] {
  return [...region.querySelectorAll<SVGPathElement>(".recharts-line-curve")].map(
    (path) => path.getAttribute("stroke") ?? "",
  );
}

function fills(region: HTMLElement): string[] {
  return [...region.querySelectorAll<SVGRectElement>(".recharts-bar-rectangle rect")].map(
    (rect) => rect.getAttribute("fill") ?? "",
  );
}

function yTicks(region: HTMLElement): string[] {
  return [
    ...region.querySelectorAll(".recharts-yAxis-tick-labels .recharts-cartesian-axis-tick-value"),
  ].map((tick) => tick.textContent ?? "");
}

it("stacks OK traces under the errors in the error colour", () => {
  renderBody();
  const region = card("Traces & errors");
  expect([...new Set(fills(region))].sort()).toEqual(["var(--chart-1)", "var(--chart-error)"]);
});

it("draws traces as bars and errors as lines per group, in each group's colour", () => {
  // Groups a and b are not in top_models, so they take the colours 1 and 2 in name order.
  render(
    <DashboardBody
      response={responseWith(
        [
          { metric: "traces", group: "a", points: points([6, 4, 3]) },
          { metric: "traces", group: "b", points: points([5, 8, 9]) },
          { metric: "errors", group: "a", points: points([1, 0, 1]) },
          { metric: "errors", group: "b", points: points([0, 2, 0]) },
        ],
        [],
      )}
      onBucket={() => {}}
    />,
  );
  const region = card("Traces & errors");
  expect([...new Set(fills(region))].sort()).toEqual(["var(--chart-1)", "var(--chart-2)"]);
  expect(strokes(region)).toEqual(["var(--chart-1)", "var(--chart-2)"]);
});

function areaFills(region: HTMLElement): string[] {
  return [...region.querySelectorAll<SVGPathElement>(".recharts-area-area")].map(
    (path) => path.getAttribute("fill") ?? "",
  );
}

function summaryOf(region: HTMLElement): string {
  return region.querySelector("[aria-live]")?.textContent ?? "";
}

function moveTo(region: HTMLElement, key: string, times = 1) {
  const group = within(region).getByRole("group");
  for (let index = 0; index < times; index += 1) {
    fireEvent.keyDown(group, { key });
  }
}

it("gives a model the same colour on every chart", () => {
  renderBody();
  // claude-3 ranks first by cost, so it is chart-1 and gpt-4o chart-2, although gpt-4o is
  // the first series in the response. Lines follow the response order: gpt-4o, then claude-3.
  expect([...new Set(areaFills(card("Cost by model")))].sort()).toEqual([
    "var(--chart-1)",
    "var(--chart-2)",
    "var(--chart-3)",
  ]);
  expect(strokes(card("LLM latency by model"))).toEqual(["var(--chart-2)", "var(--chart-1)"]);
});

it("draws the error rate axis in nice percentages", () => {
  renderBody();
  expect(yTicks(card("Error rate"))).toEqual(["0%", "5%", "10%", "15%", "20%"]);
});

it("gives tiny error rates distinct percent labels", () => {
  // A rate of 0.04%: the ticks step by a tenth of a percent, so none repeats as "0%".
  render(
    <DashboardBody
      response={responseWith([{ metric: "error_rate", points: points([0.0004, 0.0002, 0]) }])}
      onBucket={() => {}}
    />,
  );
  const labels = yTicks(card("Error rate"));
  expect(labels).toEqual(["0%", "0.1%"]);
  expect(new Set(labels).size).toBe(labels.length);
});

it("draws the latency axis in one duration unit", () => {
  renderBody();
  expect(yTicks(card("Latency"))).toEqual(["0 s", "1 s", "2 s", "3 s"]);
});

function rowsAt(times: readonly string[]): ChartRow[] {
  return times.map((time) => ({ time: Date.parse(time) }));
}

it("puts X ticks on round times of the shown zone", () => {
  // 15-minute buckets over 7h15: the one-hour step fits in eight ticks.
  const times: string[] = [];
  for (let minute = 0; minute <= 7 * 60 + 15; minute += 15) {
    times.push(new Date(Date.parse("2026-11-17T00:00:00Z") + minute * 60_000).toISOString());
  }
  const ticks = xAxisTicks(rowsAt(times), true);
  expect(ticks).toEqual(
    ["00", "01", "02", "03", "04", "05", "06", "07"].map((hour) =>
      Date.parse(`2026-11-17T${hour}:00:00Z`),
    ),
  );
});

it("falls back to every k-th bucket when no round time is a bucket", () => {
  // One bucket a day at noon: no midnight is a bucket, so every ceil(20 / 8) = 3rd one.
  const times = Array.from({ length: 20 }, (_, day) =>
    new Date(Date.parse("2026-11-01T12:00:00Z") + day * 86_400_000).toISOString(),
  );
  const ticks = xAxisTicks(rowsAt(times), true);
  expect(ticks).toEqual([0, 3, 6, 9, 12, 15, 18].map((index) => Date.parse(times[index] ?? "")));
});

it("keeps the X ticks within the label budget", () => {
  // The same 7h15 of 15-minute buckets: a budget of four labels takes the two-hour step.
  const times: string[] = [];
  for (let minute = 0; minute <= 7 * 60 + 15; minute += 15) {
    times.push(new Date(Date.parse("2026-11-17T00:00:00Z") + minute * 60_000).toISOString());
  }
  const ticks = xAxisTicks(rowsAt(times), true, 4);
  expect(ticks).toEqual(
    ["00", "02", "04", "06"].map((hour) => Date.parse(`2026-11-17T${hour}:00:00Z`)),
  );
  expect(ticks.length).toBeLessThanOrEqual(4);
});

it("keeps the every-k-th fallback within the label budget", () => {
  // Twenty daily buckets at noon and a budget of four: every ceil(20 / 4) = 5th one.
  const times = Array.from({ length: 20 }, (_, day) =>
    new Date(Date.parse("2026-11-01T12:00:00Z") + day * 86_400_000).toISOString(),
  );
  const ticks = xAxisTicks(rowsAt(times), true, 4);
  expect(ticks).toEqual([0, 5, 10, 15].map((index) => Date.parse(times[index] ?? "")));
});

it("draws at most six X labels on a card and four on a Scores mini chart", () => {
  const times = Array.from({ length: 30 }, (_, day) =>
    new Date(Date.parse("2026-11-01T12:00:00Z") + day * 86_400_000).toISOString(),
  );
  const daily = times.map((time) => ({ time, value: 1 }));
  render(
    <DashboardBody
      response={responseWith([
        { metric: "traces", points: daily },
        { metric: "score_avg", group: "alpha", points: daily },
      ])}
      onBucket={() => {}}
    />,
  );
  // Each card's X labels are the row times it shows, so a count of labels is a count of ticks.
  const cardLabels = (region: HTMLElement) =>
    region.querySelectorAll(".recharts-xAxis .recharts-cartesian-axis-tick").length;
  expect(cardLabels(card("Traces & errors"))).toBeLessThanOrEqual(6);
  expect(cardLabels(card("Scores"))).toBeLessThanOrEqual(4);
});

it("gives the legend the dashes of the p95 and p99 lines", () => {
  renderBody();
  const legend = card("Latency").querySelector(".recharts-legend-wrapper");
  expect(legend).toBeInstanceOf(Element);
  const dashes = [...(legend?.querySelectorAll("[stroke-dasharray]") ?? [])].map((node) =>
    node.getAttribute("stroke-dasharray"),
  );
  expect(dashes).toEqual(expect.arrayContaining(["4 3"]));
});

function responseWith(
  series: Schema<"MetricsResponse">["series"],
  models: Schema<"MetricsResponse">["top_models"] = RESPONSE.top_models,
): Schema<"MetricsResponse"> {
  return { ...RESPONSE, series, top_models: models };
}

it("gives a count axis whole-number ticks", () => {
  // Traces up to 2: the nice step 0.5 is skipped, so the labels are 0, 1, 2 and never repeat.
  render(
    <DashboardBody
      response={responseWith([{ metric: "traces", points: points([1, 2, 1]) }])}
      onBucket={() => {}}
    />,
  );
  expect(yTicks(card("Traces & errors"))).toEqual(["0", "1", "2"]);
});

it("reads the stacked bucket as OK traces and errors", () => {
  renderBody();
  const region = card("Traces & errors");
  moveTo(region, "ArrowRight");
  // Hour 0: 10 traces of which 2 errors, so 8 OK.
  expect(summaryOf(region)).toContain("Traces 10, Errors 2");
});

it("fades error-rate buckets with fewer than five traces and says how many", () => {
  render(
    <DashboardBody
      response={responseWith([
        { metric: "traces", points: points([3, 12, 8]) },
        { metric: "error_rate", points: points([0.16, 0, 0.04]) },
      ])}
      onBucket={() => {}}
    />,
  );
  const region = card("Error rate");
  const opacities = [...region.querySelectorAll(".recharts-bar-rectangle rect")].map((rect) =>
    rect.getAttribute("fill-opacity"),
  );
  expect(opacities).toContain("0.35");
  expect(opacities).toContain(null);
  moveTo(region, "ArrowRight");
  expect(summaryOf(region)).toContain("Error rate 16% (3 traces)");
});

it("fades each group's error rate by that group's own trace count", () => {
  render(
    <DashboardBody
      response={responseWith(
        [
          { metric: "traces", group: "a", points: points([3, 3, 3]) },
          { metric: "traces", group: "b", points: points([12, 12, 12]) },
          { metric: "error_rate", group: "a", points: points([0.2, 0.2, 0.2]) },
          { metric: "error_rate", group: "b", points: points([0.1, 0.1, 0.1]) },
        ],
        [],
      )}
      onBucket={() => {}}
    />,
  );
  const region = card("Error rate");
  const opacities = [...region.querySelectorAll(".recharts-bar-rectangle rect")].map((rect) =>
    rect.getAttribute("fill-opacity"),
  );
  // Group a's three buckets have three traces each, so they are faded; group b's are not.
  expect(opacities.filter((value) => value === "0.35")).toHaveLength(3);
  expect(opacities.filter((value) => value === null)).toHaveLength(3);
  moveTo(region, "ArrowRight");
  expect(summaryOf(region)).toContain("a 20% (3 traces)");
  expect(summaryOf(region)).toContain("b 10%");
  expect(summaryOf(region)).not.toContain("b 10% (");
});

it("draws cost by model as a stacked area", () => {
  const groups = ["m1", "m2", "m3", "m4", "m5"].map((group, index) => ({
    metric: "cost_by_model" as const,
    group,
    points: points([5 - index, 5 - index, 5 - index]),
  }));
  render(<DashboardBody response={responseWith(groups)} onBucket={() => {}} />);
  // Four areas: m1, m2, m3 and Other (m4 and m5 folded).
  const edges = areaEdges(card("Cost by model"));
  expect(edges).toHaveLength(4);
  expect(areaFills(card("Cost by model"))).toContain("var(--muted-foreground)");
  // One stack: each area's lower edge is the upper edge of the area drawn before it, and the
  // first one sits on the baseline.
  const round = (values: readonly number[]) => values.map((value) => Math.round(value * 100) / 100);
  for (let index = 1; index < edges.length; index += 1) {
    expect(round(edges[index]?.bottom ?? [])).toEqual(round(edges[index - 1]?.top ?? []));
  }
  expect(new Set(round(edges[0]?.bottom ?? [])).size).toBe(1);
});

/** The top and bottom edges (y values, left to right) of each area, from its path. */
function areaEdges(region: HTMLElement): { top: number[]; bottom: number[] }[] {
  return [...region.querySelectorAll<SVGPathElement>(".recharts-area-area")].map((path) => {
    // The path runs along the top edge left to right, then the bottom edge right to left.
    const ys = [...(path.getAttribute("d") ?? "").matchAll(/[ML][-\d.]+,([-\d.]+)/g)].map((match) =>
      Number(match[1]),
    );
    const half = ys.length / 2;
    return { top: ys.slice(0, half), bottom: ys.slice(half).reverse() };
  });
}

it("folds cost by model into the top three and other", () => {
  const groups = ["a", "b", "c", "d"].map((group, index) => ({
    metric: "cost_by_model" as const,
    group,
    points: points([10 - index, 10 - index, 10 - index]),
  }));
  render(<DashboardBody response={responseWith(groups)} onBucket={() => {}} />);
  // Three areas: the three largest groups, and `other` (the fourth) muted.
  expect(areaFills(card("Cost by model"))).toHaveLength(4);
  expect(areaFills(card("Cost by model"))).toContain("var(--muted-foreground)");
});

it("shows the scores in one chart each, sorted, with a line for more than six", () => {
  const names = ["zeta", "alpha", "beta", "gamma", "delta", "epsilon", "eta", "theta"];
  render(
    <DashboardBody
      response={responseWith(
        names.map((group) => ({
          metric: "score_avg" as const,
          group,
          points: points([0.5, 0.6, 0.7]),
        })),
      )}
      onBucket={() => {}}
    />,
  );
  const region = card("Scores");
  const headings = within(region)
    .getAllByRole("heading", { level: 3 })
    .map((heading) => heading.textContent);
  expect(headings).toEqual(["alpha", "beta", "delta", "epsilon", "eta", "gamma"]);
  expect(within(region).getByText("+2 more scores")).toBeInTheDocument();
});

function scoreChart(values: readonly number[]): HTMLElement {
  render(
    <DashboardBody
      response={responseWith([
        { metric: "score_avg", group: "alpha", points: points([...values]) },
      ])}
      onBucket={() => {}}
    />,
  );
  return card("Scores");
}

it("gives non-negative scores nice ticks in steps of at least a hundredth", () => {
  expect(yTicks(scoreChart([0.5, 0.6, 0.7]))).toEqual(["0", "0.2", "0.4", "0.6", "0.8"]);
  cleanup();
  expect(yTicks(scoreChart([0.004, 0.003, 0.002]))).toEqual(["0", "0.01"]);
});

it("leaves the axis of a Scores chart with a negative score to Recharts", () => {
  // No fixed ticks or domain: the axis shows the negative score, which a [0, top] domain would hide.
  const labels = yTicks(scoreChart([-0.4, 0.2, 0.6]));
  expect(labels.some((label) => label.startsWith("-"))).toBe(true);
});

it("says why a chart is empty in a compact card", () => {
  render(<DashboardBody response={responseWith([])} onBucket={() => {}} />);
  const region = card("Cost by model");
  expect(region).toHaveClass("xl:col-span-2");
  expect(within(region).getByText("No traces in range.")).toBeInTheDocument();
  expect(
    within(card("LLM latency by model")).getByText(/needs LLM spans with a model/),
  ).toBeInTheDocument();
  expect(
    within(card("Evaluation cost")).getByText(/No evaluation runs in range/),
  ).toBeInTheDocument();
});

it("opens the traces of a model when its top-models row is clicked", () => {
  const onModel = vi.fn();
  render(<DashboardBody response={RESPONSE} onBucket={() => {}} onModel={onModel} />);
  fireEvent.click(screen.getByRole("row", { name: /claude-3/ }));
  expect(onModel).toHaveBeenCalledWith("claude-3");
  expect(onModel).toHaveBeenCalledTimes(1);
});

it("opens a model's traces from its button, once per click, with Enter or Space", async () => {
  const onModel = vi.fn();
  render(<DashboardBody response={RESPONSE} onBucket={() => {}} onModel={onModel} />);
  const user = userEvent.setup();
  const button = screen.getByRole("button", { name: "Open traces of claude-3" });
  // The row carries no keyboard stop of its own: the model's button is the keyboard path.
  expect(screen.getByRole("row", { name: /claude-3/ })).not.toHaveAttribute("tabindex");
  await user.click(button);
  // The button's click does not bubble into the row's click: one call, not two.
  expect(onModel).toHaveBeenCalledTimes(1);
  expect(onModel).toHaveBeenLastCalledWith("claude-3");
  button.focus();
  await user.keyboard("{Enter}");
  expect(onModel).toHaveBeenCalledTimes(2);
  await user.keyboard(" ");
  expect(onModel).toHaveBeenCalledTimes(3);
  expect(onModel).toHaveBeenLastCalledWith("claude-3");
});

it("sizes each model's cost bar by its share of the listed cost", () => {
  renderBody();
  const bar = screen.getByRole("row", { name: /claude-3/ }).querySelector("div[aria-hidden]");
  // claude-3 cost 3 of the 4 listed.
  expect(bar).toHaveStyle({ width: "75%" });
});

it("shows the card titles at h2 on the flat grid of the user page", () => {
  renderBody();
  expect(within(card("Latency")).getByRole("heading", { level: 2, name: "Latency" })).toBeVisible();
});
