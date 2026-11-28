import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { DemoBar, SIGNUP_URL } from "@/components/shell/demo-bar";

it("says the data is a demo and links Start free to signup in a new tab", () => {
  render(<DemoBar />);
  expect(screen.getByRole("status")).toHaveTextContent("demo with sample data");
  const link = screen.getByRole("link", { name: "Start free" });
  expect(link).toHaveAttribute("href", SIGNUP_URL);
  expect(link).toHaveAttribute("target", "_blank");
  expect(link).toHaveAttribute("rel", "noopener noreferrer");
});
