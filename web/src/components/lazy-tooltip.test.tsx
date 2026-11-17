import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it } from "vitest";
import { LazyTooltip } from "@/components/lazy-tooltip";
import { TooltipProvider } from "@/components/ui/tooltip";

function renderTip() {
  return render(
    <TooltipProvider>
      <LazyTooltip content="Created by the SDK">
        <span>0.90</span>
      </LazyTooltip>
    </TooltipProvider>,
  );
}

it("mounts no tooltip before the user points at the child", () => {
  renderTip();
  expect(screen.getByText("0.90")).toBeInTheDocument();
  expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
  expect(screen.queryByText("Created by the SDK")).not.toBeInTheDocument();
  expect(document.querySelector("[data-slot=tooltip-content]")).toBeNull();
});

it("shows the content when the user hovers the child", async () => {
  const user = userEvent.setup();
  renderTip();
  await user.hover(screen.getByText("0.90"));
  expect(await screen.findByRole("tooltip")).toHaveTextContent("Created by the SDK");
});

it("keeps the tooltip mounted after the first hover", async () => {
  const user = userEvent.setup();
  renderTip();
  await user.hover(screen.getByText("0.90"));
  await screen.findByRole("tooltip");
  await user.unhover(screen.getByText("0.90"));
  expect(document.querySelector("[data-slot=tooltip-trigger]")).not.toBeNull();
});

it("shows the content when the child receives focus", async () => {
  render(
    <TooltipProvider>
      <LazyTooltip content="Opens the trace">
        <button type="button">Open</button>
      </LazyTooltip>
    </TooltipProvider>,
  );
  expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
  // Mounting the tooltip re-creates the child, so focus is not kept on it (see the report).
  fireEvent.focus(screen.getByRole("button", { name: "Open" }));
  expect(await screen.findByRole("tooltip")).toHaveTextContent("Opens the trace");
});
