import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { ShowMore } from "@/components/payload/show-more";

afterEach(() => {
  vi.restoreAllMocks();
});

/** Makes every element report a size, since jsdom does no layout. */
function stubHeight(height: number, width = 300) {
  vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockReturnValue(height);
  vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockReturnValue(width);
}

it("leaves short content alone", () => {
  stubHeight(40);
  render(
    <ShowMore maxHeight={100}>
      <p>short text</p>
    </ShowMore>,
  );
  expect(screen.queryByRole("button", { name: /show/i })).not.toBeInTheDocument();
});

it("clips tall content and expands it on demand", () => {
  stubHeight(900);
  render(
    <ShowMore maxHeight={100}>
      <p>long text</p>
    </ShowMore>,
  );
  const clip = screen.getByText("long text").parentElement;
  expect(clip).toHaveStyle({ maxHeight: "100px" });
  fireEvent.click(screen.getByRole("button", { name: "Show more" }));
  expect(clip).not.toHaveStyle({ maxHeight: "100px" });
  fireEvent.click(screen.getByRole("button", { name: "Show less" }));
  expect(clip).toHaveStyle({ maxHeight: "100px" });
});

it("ignores a measurement taken while the container has no width", () => {
  stubHeight(900, 0);
  render(
    <ShowMore maxHeight={100}>
      <p>text in a collapsed pane</p>
    </ShowMore>,
  );
  expect(screen.queryByRole("button", { name: /show/i })).not.toBeInTheDocument();
});
