import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { ActivityChart, activityBuckets } from "@/components/user/activity-chart";

const FROM = "2026-11-17T10:00:00.000Z";
const TO = "2026-11-17T10:04:00.000Z";
const MS0 = Date.parse(FROM);

it("spreads the values over equal buckets and leaves zeros empty", () => {
  expect(activityBuckets([0, 3, 0, 1], FROM, TO)).toEqual([
    { time: MS0, count: null },
    { time: MS0 + 60_000, count: 3 },
    { time: MS0 + 120_000, count: null },
    { time: MS0 + 180_000, count: 1 },
  ]);
});

it("has no buckets for an inverted range or no values", () => {
  expect(activityBuckets([1, 2], TO, FROM)).toEqual([]);
  expect(activityBuckets([], FROM, TO)).toEqual([]);
});

it("says there is no activity when every value is zero", () => {
  render(<ActivityChart values={[0, 0, 0]} from={FROM} to={TO} />);
  expect(screen.getByText("No activity")).toBeInTheDocument();
});
