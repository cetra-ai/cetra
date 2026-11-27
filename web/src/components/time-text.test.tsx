import { render, screen } from "@testing-library/react";
import { beforeEach, expect, it } from "vitest";
import { TIME_ZONE_STORAGE_KEY } from "@/lib/settings";
import { TimeText } from "./time-text";

const VALUE = "2026-11-19T11:51:23.072000000Z";

beforeEach(() => {
  localStorage.setItem(TIME_ZONE_STORAGE_KEY, "utc");
});

it("keeps the milliseconds in the time and datetime styles", () => {
  render(
    <>
      <TimeText value={VALUE} style="time" />
      <TimeText value={VALUE} style="datetime" />
    </>,
  );

  expect(screen.getByText("11:51:23.072")).toBeInTheDocument();
  expect(screen.getByText("2026-11-19 11:51:23.072")).toBeInTheDocument();
});

it("uses the chart axis formats in the minute and date-minute styles", () => {
  render(
    <>
      <TimeText value={VALUE} style="minute" />
      <TimeText value={VALUE} style="date-minute" />
    </>,
  );

  expect(screen.getByText("11:51")).toBeInTheDocument();
  expect(screen.getByText("11-19 11:51")).toBeInTheDocument();
  for (const time of document.querySelectorAll("time")) {
    expect(time).toHaveAttribute("dateTime", VALUE);
  }
});

it("shows an unparsable value as it is", () => {
  render(<TimeText value="not a time" style="date-minute" />);

  expect(screen.getByText("not a time")).toBeInTheDocument();
});
