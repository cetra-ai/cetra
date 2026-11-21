import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { FilterChips } from "@/components/filter-chips";

const chips = [
  { value: "a", label: "Active", count: 2, dot: "bg-status-ok" },
  { value: "b", label: "Paused" },
] as const;

describe("FilterChips", () => {
  it("shows each chip's label and count", () => {
    render(<FilterChips label="Status" chips={chips} active={undefined} onSelect={vi.fn()} />);
    expect(screen.getByRole("button", { name: "Active 2" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Paused" })).toBeInTheDocument();
  });

  it("selecting a chip filters on it", async () => {
    const onSelect = vi.fn();
    render(<FilterChips label="Status" chips={chips} active={undefined} onSelect={onSelect} />);
    await userEvent.click(screen.getByRole("button", { name: "Active 2" }));
    expect(onSelect).toHaveBeenCalledWith("a");
  });

  it("selecting the active chip clears it", async () => {
    const onSelect = vi.fn();
    render(<FilterChips label="Status" chips={chips} active="a" onSelect={onSelect} />);
    const active = screen.getByRole("button", { name: "Active 2" });
    await userEvent.click(active);
    expect(onSelect).toHaveBeenCalledWith(undefined);
    expect(active).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByRole("button", { name: "Paused" })).toHaveAttribute("aria-pressed", "false");
  });

  it("renders a dot only when given", () => {
    const { container } = render(
      <FilterChips label="Status" chips={chips} active={undefined} onSelect={vi.fn()} />,
    );
    const withDot = screen.getByRole("button", { name: "Active 2" });
    const withoutDot = screen.getByRole("button", { name: "Paused" });
    expect(withDot.querySelectorAll("span.rounded-full")).toHaveLength(1);
    expect(withoutDot.querySelectorAll("span.rounded-full")).toHaveLength(0);
    expect(container.querySelectorAll("span.rounded-full")).toHaveLength(1);
  });

  it("labels its group", () => {
    render(<FilterChips label="Status" chips={chips} active={undefined} onSelect={vi.fn()} />);
    expect(screen.getByRole("region", { name: "Status" })).toBeInTheDocument();
  });
});
