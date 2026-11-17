import { fireEvent, render, screen, within } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { formatCost } from "@/lib/format";
import { TopUsers } from "./top-users";

function user(overrides: Partial<Schema<"EndUserSummary">>): Schema<"EndUserSummary"> {
  return {
    user_id: "user-a",
    first_seen: "2026-01-01T10:00:00.000000000Z",
    last_seen: "2026-01-01T10:20:03.000000000Z",
    trace_count: 4,
    session_count: 2,
    error_count: 0,
    error_trace_count: 0,
    activity: Array.from({ length: 24 }, () => 0),
    scores: [],
    ...overrides,
  };
}

it("lists the users with a cost, in the given order, with their names and costs", () => {
  render(
    <TopUsers
      users={[
        user({ user_id: "user-a", cost_total: 2, metadata: { name: '"Grace Test"' } }),
        user({ user_id: "user-b", cost_total: 1 }),
        user({ user_id: "user-c", cost_total: 0 }),
      ]}
      onSelect={() => undefined}
    />,
  );
  const section = screen.getByRole("region", { name: "Top users by cost" });
  expect(
    within(section)
      .getAllByRole("button")
      .map((button) => button.getAttribute("aria-label")),
  ).toEqual([`Grace Test: ${formatCost(2)}`, `user-b: ${formatCost(1)}`]);
  expect(within(section).getByText("Grace Test")).toBeInTheDocument();
  expect(within(section).getByText("user-b")).toHaveClass("font-mono");
});

it("renders nothing when no user has a cost", () => {
  const { container } = render(
    <TopUsers
      users={[user({ cost_total: 0 }), user({ user_id: "user-b" })]}
      onSelect={() => undefined}
    />,
  );
  expect(container).toBeEmptyDOMElement();
});

it("renders nothing for an empty list", () => {
  const { container } = render(<TopUsers users={[]} onSelect={() => undefined} />);
  expect(container).toBeEmptyDOMElement();
});

it("calls onSelect with the user id when a user is clicked", () => {
  const onSelect = vi.fn();
  render(<TopUsers users={[user({ user_id: "user-b", cost_total: 1 })]} onSelect={onSelect} />);
  fireEvent.click(screen.getByRole("button", { name: `user-b: ${formatCost(1)}` }));
  expect(onSelect).toHaveBeenCalledWith("user-b");
});
