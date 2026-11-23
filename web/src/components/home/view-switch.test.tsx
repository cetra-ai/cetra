import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { ProjectViewSwitch } from "@/components/home/view-switch";
import { TooltipProvider } from "@/components/ui/tooltip";
import { PROJECT_VIEW_STORAGE_KEY, setProjectView } from "@/lib/org-home";

function renderSwitch() {
  return render(
    <TooltipProvider>
      <ProjectViewSwitch />
    </TooltipProvider>,
  );
}

describe("project view switch", () => {
  afterEach(() => {
    // The store keeps an in-memory fallback that outlives localStorage.clear().
    setProjectView("cards");
  });

  it("checks Cards by default", () => {
    renderSwitch();
    expect(screen.getByRole("radiogroup", { name: "View" })).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: "Cards" })).toHaveAttribute("aria-checked", "true");
    expect(screen.getByRole("radio", { name: "Table" })).toHaveAttribute("aria-checked", "false");
  });

  it("clicking Table checks it and stores it", () => {
    renderSwitch();
    fireEvent.click(screen.getByRole("radio", { name: "Table" }));
    expect(screen.getByRole("radio", { name: "Table" })).toHaveAttribute("aria-checked", "true");
    expect(screen.getByRole("radio", { name: "Cards" })).toHaveAttribute("aria-checked", "false");
    expect(localStorage.getItem(PROJECT_VIEW_STORAGE_KEY)).toBe("table");
  });
});
