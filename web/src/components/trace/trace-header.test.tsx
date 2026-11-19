import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { renderApp } from "@/test/render";

const TRACE = "a1000000000000000000000000000001";

describe("trace header previous and next", () => {
  it("has no previous or next buttons on the full page, which has no list", async () => {
    await renderApp(`/acme/support-bot/traces/${TRACE}`);
    const header = await screen.findByRole("region", { name: "Trace" });
    expect(
      within(header).queryByRole("button", { name: "Previous trace" }),
    ).not.toBeInTheDocument();
    expect(within(header).queryByRole("button", { name: "Next trace" })).not.toBeInTheDocument();
  });
});
