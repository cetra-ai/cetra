import { waitFor } from "@testing-library/react";
import { expect, it } from "vitest";
import { renderApp } from "@/test/render";

const BASELINE_ID = "01900000-0000-7000-8000-000000000e01";
const ITEM = "01900000-0000-7000-8000-00000000cc01:1";

it("opens the list with the experiment's panel when visiting an experiment URL", async () => {
  const { router } = await renderApp(
    `/acme/support-bot/experiments/${BASELINE_ID}?errors=1&item=${ITEM}`,
  );

  await waitFor(() => expect(router.state.location.pathname).toBe("/acme/support-bot/experiments"));
  const href = decodeURIComponent(router.state.location.href);
  expect(href).toContain(`experiment=${BASELINE_ID}`);
  expect(href).toContain("errors=1");
  expect(href).toContain(`item=${ITEM}`);
  await waitFor(() => expect(router.state.status).toBe("idle"));
});
