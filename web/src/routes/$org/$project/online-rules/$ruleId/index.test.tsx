import { screen, waitFor, within } from "@testing-library/react";
import { expect, it } from "vitest";
import { renderApp } from "@/test/render";
import ruleFixture from "../../../../../../../fixtures/api/static/online-rule.json";

const RULE_ID = "01900000-0000-7000-8000-000000000700";

it("opens the list with the rule's panel when visiting a rule URL", async () => {
  const { router } = await renderApp(`/acme/support-bot/online-rules/${RULE_ID}`);

  const panel = await screen.findByRole("region", { name: "Online rule" });
  expect(await within(panel).findByText(ruleFixture.name)).toBeInTheDocument();
  expect(router.state.location.pathname).toBe("/acme/support-bot/online-rules");
  expect(router.state.location.search).toMatchObject({ rule: RULE_ID });
  await waitFor(() => expect(router.state.status).toBe("idle"));
});
