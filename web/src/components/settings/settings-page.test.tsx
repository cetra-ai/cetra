import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { SettingsPage } from "@/components/settings/settings-page";

function column(): HTMLElement {
  const heading = screen.getByRole("heading", { level: 1 });
  // The heading sits in the title row, inside the width-limited column.
  return heading.closest("div.mx-auto") as HTMLElement;
}

it("keeps form pages at the narrow reading width", () => {
  render(
    <SettingsPage title="General">
      <p>Form</p>
    </SettingsPage>,
  );
  expect(column()).toHaveClass("max-w-5xl");
});

it("lets list pages use the full width", () => {
  render(
    <SettingsPage title="API keys" width="wide">
      <p>Table</p>
    </SettingsPage>,
  );
  expect(column()).toHaveClass("max-w-none");
  expect(column()).not.toHaveClass("max-w-5xl");
});
