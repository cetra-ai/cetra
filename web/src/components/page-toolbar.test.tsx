import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import {
  ITEM_TITLE_CLASS,
  PageToolbar,
  PageToolbarSummary,
  PageToolbarTitle,
} from "@/components/page-toolbar";

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

describe("PageToolbarTitle", () => {
  it("shows the back link, a slash and the name", () => {
    render(
      <PageToolbarTitle back={<a href="/list">List</a>} label="Item">
        <h2 className={ITEM_TITLE_CLASS}>Item</h2>
      </PageToolbarTitle>,
    );
    const nav = screen.getByRole("navigation", { name: "Item" });
    const link = within(nav).getByRole("link", { name: "List" });
    const heading = within(nav).getByRole("heading", { level: 2, name: "Item" });

    expect(link.compareDocumentPosition(heading) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(nav).toHaveTextContent(/^List\/Item$/);
  });

  it("takes a nav label", () => {
    render(
      <PageToolbarTitle back={<a href="/queues">Queues</a>} label="Annotation queue">
        <h2 className={ITEM_TITLE_CLASS}>Review</h2>
      </PageToolbarTitle>,
    );
    expect(screen.getByRole("navigation", { name: "Annotation queue" })).toBeInTheDocument();
  });
});
