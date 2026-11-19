import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { StatusPill } from "./status-pill";

it("uses the tone's status colours", () => {
  render(<StatusPill tone="error">Failing</StatusPill>);

  const pill = screen.getByText("Failing");
  expect(pill).toHaveClass("text-status-error", "border-status-error/30", "bg-status-error/10");
});

it("shows a title when given one", () => {
  render(
    <StatusPill tone="warn" title="SMTP not configured">
      Not configured
    </StatusPill>,
  );

  expect(screen.getByTitle("SMTP not configured")).toHaveTextContent("Not configured");
});
