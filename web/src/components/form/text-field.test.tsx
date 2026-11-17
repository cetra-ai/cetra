import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import { TextField } from "./text-field";

it("shows the unit after the input", () => {
  render(
    <TextField
      id="retention"
      label="Retention"
      value="30"
      onChange={vi.fn()}
      unit="days"
      inputMode="numeric"
      min={1}
    />,
  );

  const input = screen.getByLabelText("Retention");
  expect(input).toHaveValue("30");
  expect(input).toHaveAttribute("inputmode", "numeric");
  expect(input).toHaveAttribute("min", "1");
  expect(screen.getByText("days")).toBeInTheDocument();
});

it("renders no unit without the prop", () => {
  render(<TextField id="name" label="Name" value="acme" onChange={vi.fn()} />);

  expect(screen.getByLabelText("Name")).toHaveValue("acme");
  expect(screen.queryByText("days")).toBeNull();
});

it("reports each change", async () => {
  const user = userEvent.setup();
  const onChange = vi.fn();
  render(<TextField id="timeout" label="Timeout" value="" onChange={onChange} unit="seconds" />);

  await user.type(screen.getByLabelText("Timeout"), "6");
  expect(onChange).toHaveBeenCalledWith("6");
});
