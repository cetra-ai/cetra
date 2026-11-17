import { act, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, expect, it, vi } from "vitest";
import { ItemSearch } from "@/components/datasets/items-toolbar";

afterEach(() => {
  vi.useRealTimers();
});

function Host() {
  const [value, setValue] = useState<string | undefined>("first");
  return (
    <>
      <ItemSearch value={value} onChange={setValue} />
      <button
        type="button"
        onClick={() => {
          setValue("second");
        }}
      >
        Other
      </button>
    </>
  );
}

it("applies the trimmed text once the typing pauses", async () => {
  vi.useFakeTimers();
  const onChange = vi.fn();
  render(<ItemSearch value={undefined} onChange={onChange} />);
  const box = screen.getByRole("searchbox", { name: "Search items" });

  fireEvent.change(box, { target: { value: "  refund " } });
  expect(onChange).not.toHaveBeenCalled();
  await act(async () => {
    vi.advanceTimersByTime(300);
  });

  expect(onChange).toHaveBeenCalledTimes(1);
  expect(onChange).toHaveBeenCalledWith("refund");
});

it("applies a blank box as no search", async () => {
  vi.useFakeTimers();
  const onChange = vi.fn();
  render(<ItemSearch value="refund" onChange={onChange} />);
  const box = screen.getByRole("searchbox", { name: "Search items" });

  fireEvent.change(box, { target: { value: "   " } });
  await act(async () => {
    vi.advanceTimersByTime(300);
  });

  expect(onChange).toHaveBeenCalledWith(undefined);
});

it("applies the text at once on Enter", async () => {
  const user = userEvent.setup();
  const onChange = vi.fn();
  render(<ItemSearch value={undefined} onChange={onChange} />);

  await user.type(screen.getByRole("searchbox", { name: "Search items" }), "billing{Enter}");

  expect(onChange).toHaveBeenLastCalledWith("billing");
});

it("caps the box at 200 characters, the most the server takes", () => {
  render(<ItemSearch value={undefined} onChange={() => undefined} />);

  expect(screen.getByRole("searchbox", { name: "Search items" })).toHaveAttribute(
    "maxlength",
    "200",
  );
});

it("shows the applied text when it changes from outside", async () => {
  const user = userEvent.setup();
  render(<Host />);
  const box = screen.getByRole("searchbox", { name: "Search items" });
  expect(box).toHaveValue("first");

  await user.click(screen.getByRole("button", { name: "Other" }));

  expect(box).toHaveValue("second");
});
