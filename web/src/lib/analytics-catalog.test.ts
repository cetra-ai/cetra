import { QueryClient } from "@tanstack/react-query";
import { createMemoryHistory } from "@tanstack/react-router";
import { expect, it } from "vitest";
import { WEB_EVENT_PROPERTIES } from "@/lib/analytics";
import { createAppRouter } from "@/router";
import golden from "../../../fixtures/analytics/catalog.json";

/** The public pages, which the signed-in UI's catalog does not list as page routes. */
const PUBLIC_ROUTE_IDS = ["/login", "/signup", "/reset", "/verify", "/setup/", "/invite/$token"];

it("web events match the golden catalog", () => {
  const web = golden.events
    .filter((event) => event.source === "web")
    .map((event) => [event.name, event.properties] as const)
    .sort(([a], [b]) => a.localeCompare(b));
  expect(web).toEqual(Object.entries(WEB_EVENT_PROPERTIES).sort(([a], [b]) => a.localeCompare(b)));
});

it("page routes match the router", () => {
  const router = createAppRouter({
    queryClient: new QueryClient(),
    history: createMemoryHistory(),
  });
  const leaves = Object.values(router.routesById)
    .filter((route) => route.id !== "__root__")
    .filter((route) => {
      const children = (route as { children?: unknown }).children;
      return (
        children === undefined ||
        (Array.isArray(children)
          ? children.length === 0
          : Object.keys(children as object).length === 0)
      );
    })
    .map((route) => route.id)
    .filter((id) => !PUBLIC_ROUTE_IDS.includes(id))
    .sort();
  expect(leaves).toEqual([...golden.page_routes].sort());
});
