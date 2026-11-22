import { render } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { useEffect } from "react";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { recordRequests } from "@/test/requests";

const PROBE = "/api/v1/teardown-probe";

/** Sends a request when it unmounts, as the reviewer releases its lease. */
function SendsOnUnmount() {
  useEffect(
    () => () => {
      void globalThis
        .fetch(`${window.location.origin}${PROBE}`, { method: "DELETE" })
        .catch(() => undefined);
    },
    [],
  );
  return null;
}

// The two tests run in order: the first leaves its unmount to the shared teardown.
it("a page that sends a request on unmount", () => {
  server.use(http.delete(PROBE, () => new HttpResponse(null, { status: 204 })));
  render(<SendsOnUnmount />);
});

it("the next test records nothing from the previous test's teardown", async () => {
  const requests = recordRequests();
  await new Promise((resolve) => setTimeout(resolve, 50));
  requests.stop();
  expect(requests.calls).toEqual([]);
});
