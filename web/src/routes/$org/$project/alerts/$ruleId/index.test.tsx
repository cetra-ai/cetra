import { screen, waitFor, within } from "@testing-library/react";
import { expect, it } from "vitest";
import { FIRING_RULE_ID } from "@/test/fixtures";
import { renderApp } from "@/test/render";
import ruleFixture from "../../../../../../../fixtures/api/static/alert-rule.json";

it("opens the list with the rule's panel when visiting a rule URL", async () => {
  const { router } = await renderApp(`/acme/support-bot/alerts/${FIRING_RULE_ID}`);

  const panel = await screen.findByRole("region", { name: "Alert rule" });
  expect(await within(panel).findByText(ruleFixture.name)).toBeInTheDocument();
  expect(router.state.location.pathname).toBe("/acme/support-bot/alerts");
  expect(router.state.location.search).toMatchObject({ rule: FIRING_RULE_ID });
  await waitFor(() => expect(router.state.status).toBe("idle"));
});
