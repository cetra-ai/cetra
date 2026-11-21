import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { PageToolbar, PageToolbarSummary } from "@/components/page-toolbar";

describe("PageToolbar", () => {
  it("renders its children before the actions", () => {
    const { container } = render(
      <PageToolbar actions={<button type="button">New</button>}>
        <input aria-label="Search" />
      </PageToolbar>,
    );
    const search = screen.getByRole("textbox", { name: "Search" });
    const newButton = screen.getByRole("button", { name: "New" });

    expect(
      search.compareDocumentPosition(newButton) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
    expect(newButton.parentElement).toHaveClass("ml-auto");
    expect(container.querySelector('[data-slot="page-toolbar"]')).toBeInTheDocument();
  });

  it("renders no action group without actions", () => {
    const { container } = render(
      <PageToolbar>
        <input aria-label="Search" />
      </PageToolbar>,
    );
    expect(container.querySelector(".ml-auto")).toBeNull();
  });
});

describe("PageToolbarSummary", () => {
  it("shows its children", () => {
    render(<PageToolbarSummary>3 evaluators</PageToolbarSummary>);
    expect(screen.getByText("3 evaluators")).toBeInTheDocument();
  });
});
