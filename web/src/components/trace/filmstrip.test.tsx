import { fireEvent, render, screen } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import { Filmstrip } from "@/components/trace/filmstrip";

const items = [
  { id: "a", label: "alpha", detail: "1.0s", status: "ok" as const },
  { id: "b", label: "beta", detail: "2.0s", status: "error" as const },
  { id: "c", label: "gamma", detail: "3.0s", status: "ok" as const },
];

it("marks the selected row and steps to the neighbours", () => {
  const onSelect = vi.fn();
  render(<Filmstrip items={items} selectedId="b" onSelect={onSelect} onBack={() => {}} />);
  expect(screen.getByRole("button", { name: /beta/ })).toHaveAttribute("aria-current", "true");
  fireEvent.click(screen.getByRole("button", { name: "Previous" }));
  expect(onSelect).toHaveBeenLastCalledWith("a");
  fireEvent.click(screen.getByRole("button", { name: "Next" }));
  expect(onSelect).toHaveBeenLastCalledWith("c");
});

it("disables Previous on the first row and Next on the last", () => {
  const { rerender } = render(
    <Filmstrip items={items} selectedId="a" onSelect={() => {}} onBack={() => {}} />,
  );
  expect(screen.getByRole("button", { name: "Previous" })).toBeDisabled();
  rerender(<Filmstrip items={items} selectedId="c" onSelect={() => {}} onBack={() => {}} />);
  expect(screen.getByRole("button", { name: "Next" })).toBeDisabled();
});

it("selects a row from its chip and goes back to the list", () => {
  const onSelect = vi.fn();
  const onBack = vi.fn();
  render(<Filmstrip items={items} selectedId="a" onSelect={onSelect} onBack={onBack} />);
  fireEvent.click(screen.getByRole("button", { name: /gamma/ }));
  expect(onSelect).toHaveBeenCalledWith("c");
  fireEvent.click(screen.getByRole("button", { name: /List/ }));
  expect(onBack).toHaveBeenCalled();
});

it("scrolls the strip sideways with the mouse wheel", () => {
  render(<Filmstrip items={items} selectedId="a" onSelect={() => {}} onBack={() => {}} />);
  const strip = screen.getByRole("button", { name: /alpha/ }).parentElement;
  if (!strip) {
    throw new Error("missing strip");
  }
  strip.scrollLeft = 0;
  const wheel = new WheelEvent("wheel", { deltaY: 40, cancelable: true, bubbles: true });
  strip.dispatchEvent(wheel);
  expect(strip.scrollLeft).toBe(40);
  expect(wheel.defaultPrevented).toBe(true);
});
