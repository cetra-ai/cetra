import { render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { ActivitySparkline, sparklineRows } from "@/components/home/activity-sparkline";

const FROM = "2025-12-31T12:00:00.000000000Z";

describe("sparklineRows", () => {
  it("starts each bucket an hour apart", () => {
    const rows = sparklineRows(FROM, [
      { traces: 1, errors: 0 },
      { traces: 1, errors: 0 },
    ]);
    expect(rows.map((row) => row.time)).toEqual([Date.parse(FROM), Date.parse(FROM) + 3_600_000]);
  });

  it("splits OK and errors and nulls zeros", () => {
    const rows = sparklineRows(FROM, [
      { traces: 4, errors: 1 },
      { traces: 0, errors: 0 },
    ]);
    expect(rows[0]).toMatchObject({ ok: 3, errors: 1 });
    expect(rows[1]).toMatchObject({ ok: null, errors: null });
  });

  it("never draws more errors than traces", () => {
    const rows = sparklineRows(FROM, [{ traces: 1, errors: 5 }]);
    expect(rows[0]).toMatchObject({ ok: null, errors: 1 });
  });

  it("is empty for an unparseable from", () => {
    expect(sparklineRows("not a time", [{ traces: 1, errors: 0 }])).toEqual([]);
  });
});

describe("ActivitySparkline", () => {
  it("draws one segment per non-zero part", async () => {
    render(
      <ActivitySparkline
        from={FROM}
        buckets={[
          { traces: 2, errors: 0 },
          { traces: 0, errors: 0 },
          { traces: 3, errors: 1 },
        ]}
      />,
    );
    const chart = screen.getByRole("img", { name: "Traces in the last 24 hours" });
    // Recharts also renders a zero-height rect for each empty segment; those are not drawn.
    await waitFor(() => {
      const drawn = [...chart.querySelectorAll(".recharts-bar-rectangle")].filter((bar) => {
        const rect = bar.querySelector("rect");
        return rect === null || Number(rect.getAttribute("height")) > 0;
      });
      expect(drawn).toHaveLength(3);
    });
  });
});
