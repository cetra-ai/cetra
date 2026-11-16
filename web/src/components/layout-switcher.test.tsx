import { fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { LayoutSwitcher } from "@/components/layout-switcher";
import { TooltipProvider } from "@/components/ui/tooltip";
import { LIST_LAYOUT_STORAGE_KEY, setListLayout } from "@/lib/list-layout";

function renderSwitcher() {
  return render(
    <TooltipProvider>
      <LayoutSwitcher />
    </TooltipProvider>,
  );
}

describe("layout switcher", () => {
  afterEach(() => {
    // The store keeps an in-memory fallback that outlives localStorage.clear().
    setListLayout("drawer");
  });

  it("renders three radios with Drawer checked by default", () => {
    renderSwitcher();
    const group = screen.getByRole("radiogroup", { name: "Layout" });
    const radios = within(group).getAllByRole("radio");
    expect(radios.map((radio) => radio.getAttribute("aria-label"))).toEqual([
      "Drawer",
      "Bottom",
      "Page",
    ]);
    expect(screen.getByRole("radio", { name: "Drawer" })).toHaveAttribute("aria-checked", "true");
    expect(screen.getByRole("radio", { name: "Page" })).toHaveAttribute("aria-checked", "false");
  });

  it("checks the clicked layout and stores it", () => {
    renderSwitcher();
    fireEvent.click(screen.getByRole("radio", { name: "Bottom" }));
    expect(screen.getByRole("radio", { name: "Bottom" })).toHaveAttribute("aria-checked", "true");
    expect(screen.getByRole("radio", { name: "Drawer" })).toHaveAttribute("aria-checked", "false");
    expect(localStorage.getItem(LIST_LAYOUT_STORAGE_KEY)).toBe("bottom");
  });
});
