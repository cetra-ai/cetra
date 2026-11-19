import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { Switch } from "@/components/ui/switch";

describe("Switch", () => {
  it("sizes the default thumb in px to match the track", () => {
    render(<Switch aria-label="x" />);
    const thumb = document.querySelector("[data-slot=switch-thumb]");
    expect(thumb).toHaveClass("group-data-[size=default]/switch:size-[16px]");
    expect(thumb).not.toHaveClass("size-4");
    expect(thumb).not.toHaveClass("size-3");
  });

  it("sizes the sm thumb in px to match the track", () => {
    render(<Switch size="sm" aria-label="y" />);
    const thumb = screen
      .getByRole("switch", { name: "y" })
      .querySelector("[data-slot=switch-thumb]");
    expect(thumb).toHaveClass("group-data-[size=sm]/switch:size-[12px]");
    expect(thumb).not.toHaveClass("size-4");
    expect(thumb).not.toHaveClass("size-3");
  });
});
