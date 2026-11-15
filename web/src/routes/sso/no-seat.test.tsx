import { screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { renderApp } from "@/test/render";

it("says there is no seat in the org named in the URL", async () => {
  await renderApp("/sso/no-seat?org=acme");
  expect(screen.getByRole("status")).toHaveTextContent("No seat available in acme; ask an owner.");
  expect(screen.getByRole("heading", { name: "No seat available", level: 1 })).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Continue" })).toHaveAttribute("href", "/");
});

it("shows the generic text for an org value that is not slug-shaped", async () => {
  await renderApp("/sso/no-seat?org=Visit%20evil.example%20now");
  expect(screen.getByRole("status")).toHaveTextContent(
    "No seat available in this organization; ask an owner.",
  );
});

it("says there is no seat in this organization without an org", async () => {
  await renderApp("/sso/no-seat");
  expect(screen.getByRole("status")).toHaveTextContent(
    "No seat available in this organization; ask an owner.",
  );
  expect(screen.getByRole("link", { name: "Continue" })).toHaveAttribute("href", "/");
});
