import { act, fireEvent, render, screen } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import { ReadOnlyCodeField } from "./read-only-code-field";

it("shows the value in a code block with a copy button", async () => {
  const writeText = vi.fn().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });
  render(
    <ReadOnlyCodeField
      id="project-slug"
      label="Slug"
      value="bot"
      description="Part of every URL of this project; cannot be changed."
    />,
  );

  const code = screen.getByText("bot");
  expect(code.tagName).toBe("CODE");
  expect(code).toHaveAttribute("id", "project-slug");
  expect(screen.getByText("Part of every URL of this project; cannot be changed.")).toBeVisible();

  fireEvent.click(screen.getByRole("button", { name: "Copy slug" }));
  await act(async () => {
    await Promise.resolve();
  });
  expect(writeText).toHaveBeenCalledWith("bot");
});
