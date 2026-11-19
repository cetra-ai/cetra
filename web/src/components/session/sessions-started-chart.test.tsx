import { render, screen, waitFor } from "@testing-library/react";
import { expect, it } from "vitest";
import { SessionsStartedChart, startedBuckets } from "@/components/session/sessions-started-chart";

const FROM = "2026-11-17T10:00:00.000Z";
// 24 minutes, so each bucket is one minute.
const TO = "2026-11-17T10:24:00.000Z";
const MS0 = Date.parse(FROM);
const MINUTE_MS = 60 * 1000;

function at(time: string) {
  return { start_time: time };
}

it("counts sessions per bucket and leaves empty buckets null", () => {
  const rows = startedBuckets(
    [
      at("2026-11-17T10:00:30.000Z"),
      at("2026-11-17T10:00:45.000Z"),
      at("2026-11-17T10:05:00.000Z"),
      at("2026-11-17T10:23:59.000Z"),
    ],
    FROM,
    TO,
  );
  expect(rows).toHaveLength(24);
  expect(rows[0]).toEqual({ time: MS0, count: 2 });
  expect(rows[5]).toEqual({ time: MS0 + 5 * MINUTE_MS, count: 1 });
  expect(rows[23]).toEqual({ time: MS0 + 23 * MINUTE_MS, count: 1 });
  expect(rows.filter((row) => row.count !== null)).toHaveLength(3);
});

it("counts a start exactly at the end of the range in the last bucket and skips sessions outside it", () => {
  const rows = startedBuckets(
    [at(TO), at("2026-11-17T09:59:59.000Z"), at("2026-11-17T10:24:01.000Z"), at("not a time")],
    FROM,
    TO,
  );
  expect(rows.filter((row) => row.count !== null)).toEqual([
    { time: MS0 + 23 * MINUTE_MS, count: 1 },
  ]);
});

it("returns no buckets for an empty or inverted range", () => {
  expect(startedBuckets([at(FROM)], TO, FROM)).toEqual([]);
  expect(startedBuckets([at(FROM)], FROM, FROM)).toEqual([]);
});

it("draws one bar per non-empty bucket", async () => {
  render(
    <SessionsStartedChart
      sessions={[
        at("2026-11-17T10:00:30.000Z"),
        at("2026-11-17T10:05:00.000Z"),
        at("2026-11-17T10:23:59.000Z"),
      ]}
      from={FROM}
      to={TO}
    />,
  );
  const chart = screen.getByRole("img", { name: "Sessions started" });
  expect(chart).toHaveClass("h-16");
  await waitFor(() => {
    expect(chart.querySelectorAll(".recharts-bar-rectangle")).toHaveLength(3);
  });
});
