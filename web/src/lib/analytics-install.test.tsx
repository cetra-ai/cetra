import { HttpResponse, http } from "msw";
import { afterEach, expect, it, vi } from "vitest";
import type { WireEvent } from "@/lib/analytics";
import { installAnalytics } from "@/lib/analytics-install";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import authConfig from "../../../fixtures/api/static/auth-config.json";

const uninstalls: Array<() => void> = [];

afterEach(() => {
  for (const uninstall of uninstalls.splice(0)) {
    uninstall();
  }
});

function analyticsOn(): void {
  server.use(
    http.get("/api/auth/config", () => HttpResponse.json({ ...authConfig, analytics: true })),
  );
}

it("installs nothing when analytics is off", async () => {
  const { router, queryClient } = await renderApp("/acme/support-bot/traces");
  const send = vi.fn(async (_events: WireEvent[]) => {});
  uninstalls.push(await installAnalytics(router, queryClient, { send, flushMs: 10 }));
  await router.navigate({
    to: "/$org/$project/sessions",
    params: { org: "acme", project: "support-bot" },
  });
  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(send).not.toHaveBeenCalled();
});

it("sends page views by route template", async () => {
  analyticsOn();
  const { router, queryClient } = await renderApp("/acme/support-bot/traces");
  const sent: WireEvent[] = [];
  const send = vi.fn(async (events: WireEvent[]) => {
    sent.push(...events);
  });
  uninstalls.push(await installAnalytics(router, queryClient, { send, flushMs: 10 }));

  await vi.waitFor(() =>
    expect(sent).toContainEqual({
      name: "page.viewed",
      org: "acme",
      project: "support-bot",
      properties: { route: "/$org/$project/traces/" },
    }),
  );

  await router.navigate({
    to: "/$org/$project/sessions",
    params: { org: "acme", project: "support-bot" },
  });
  await vi.waitFor(() =>
    expect(sent).toContainEqual({
      name: "page.viewed",
      org: "acme",
      project: "support-bot",
      properties: { route: "/$org/$project/sessions/" },
    }),
  );

  for (const event of sent) {
    expect(JSON.stringify(event.properties ?? {})).not.toContain("acme");
    expect(JSON.stringify(event.properties ?? {})).not.toContain("support-bot");
  }
});

it("a search change is not a page view", async () => {
  analyticsOn();
  const { router, queryClient } = await renderApp("/acme/support-bot/traces");
  const sent: WireEvent[] = [];
  const send = vi.fn(async (events: WireEvent[]) => {
    sent.push(...events);
  });
  uninstalls.push(await installAnalytics(router, queryClient, { send, flushMs: 10 }));
  await vi.waitFor(() => expect(sent.filter((e) => e.name === "page.viewed")).toHaveLength(1));

  await router.navigate({
    to: "/$org/$project/traces",
    params: { org: "acme", project: "support-bot" },
    search: { q: "error" },
  });
  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(sent.filter((e) => e.name === "page.viewed")).toHaveLength(1);
});

it("public pages send no page view", async () => {
  analyticsOn();
  const { router, queryClient } = await renderApp("/login");
  const sent: WireEvent[] = [];
  const send = vi.fn(async (events: WireEvent[]) => {
    sent.push(...events);
  });
  uninstalls.push(await installAnalytics(router, queryClient, { send, flushMs: 10 }));
  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(sent.filter((e) => e.name === "page.viewed")).toHaveLength(0);
});

it("the config failing leaves analytics off", async () => {
  server.use(http.get("/api/auth/config", () => HttpResponse.json({}, { status: 500 })));
  const { router, queryClient } = await renderApp("/acme/support-bot/traces");
  const send = vi.fn(async (_events: WireEvent[]) => {});
  uninstalls.push(await installAnalytics(router, queryClient, { send, flushMs: 10 }));
  await new Promise((resolve) => setTimeout(resolve, 100));
  expect(send).not.toHaveBeenCalled();
});
