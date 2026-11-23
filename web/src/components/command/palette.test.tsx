import { screen, waitFor, within } from "@testing-library/react";
import { expect, it } from "vitest";
import { startAnalytics, type WireEvent } from "@/lib/analytics";
import { SHORTCUTS, shortcutLabel } from "@/lib/shortcuts";
import { renderApp } from "@/test/render";

const TRACE = "a1000000000000000000000000000001";
const SPAN = "b100000000000013";

async function openPalette(user: Awaited<ReturnType<typeof renderApp>>["user"]) {
  await user.keyboard("{Meta>}k{/Meta}");
  return screen.findByRole("dialog", { name: /Command palette/ });
}

it("opening the palette is tracked once", async () => {
  const sent: WireEvent[][] = [];
  const stop = startAnalytics({
    send: async (events) => {
      sent.push(events);
    },
    flushMs: 60_000,
  });
  try {
    const { user } = await renderApp("/acme/support-bot/traces");
    await screen.findByRole("grid", { name: "Traces" });
    await openPalette(user);
  } finally {
    // Nothing is sent before the 60 s interval, so `stop` sends the queue.
    stop();
  }
  expect(sent.flat().map((event) => event.name)).toEqual(["command_palette.opened"]);
});

it("opens the palette with Ctrl/Cmd+K and shows the platform's key", async () => {
  const { user } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  expect(screen.getByText("Ctrl+K")).toBeInTheDocument();
  const dialog = await openPalette(user);
  expect(within(dialog).getByRole("combobox")).toBeInTheDocument();
});

it("navigates to a project by name", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  const dialog = await openPalette(user);
  // "Playground" is also a page of the project, so pick the project's own row.
  await user.type(within(dialog).getByRole("combobox"), "Playground");
  const projects = within(dialog).getByRole("group", { name: "Projects" });
  await user.click(within(projects).getByRole("option", { name: /Playground/ }));
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/playground/traces");
  });
});

it("opens a pasted trace id on the full page", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  const dialog = await openPalette(user);
  await user.click(within(dialog).getByRole("combobox"));
  await user.paste(TRACE);
  expect(
    await within(dialog).findByRole("option", { name: `Open trace ${TRACE}` }),
  ).toBeInTheDocument();
  await user.keyboard("{Enter}");
  await waitFor(() => {
    expect(router.state.location.pathname).toBe(`/acme/support-bot/traces/${TRACE}`);
  });
});

it("sends a span id to the spans list", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  const dialog = await openPalette(user);
  await user.click(within(dialog).getByRole("combobox"));
  await user.paste(SPAN);
  expect(
    await within(dialog).findByRole("option", { name: `Find span ${SPAN}` }),
  ).toBeInTheDocument();
  await user.keyboard("{Enter}");
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/spans");
    expect(router.state.location.search).toMatchObject({ filter: `span_id = '${SPAN}'` });
  });
});

it("lists a trace after it is opened", async () => {
  const { user } = await renderApp("/acme/support-bot/traces");
  const list = await screen.findByRole("grid", { name: "Traces" });
  await user.click(within(list).getByRole("row", { name: /4\.20s/ }));
  await screen.findByRole("grid", { name: "Trace" });
  const dialog = await openPalette(user);
  expect(await within(dialog).findByRole("option", { name: "support-agent" })).toBeInTheDocument();
});

it("opens the shortcut help for every shortcut", async () => {
  const { user } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  await user.keyboard("?");
  const dialog = await screen.findByRole("dialog", { name: "Keyboard shortcuts" });
  for (const row of SHORTCUTS) {
    expect(
      within(dialog).getAllByText(shortcutLabel(row.keys, navigator.userAgent)).length,
    ).toBeGreaterThan(0);
    expect(within(dialog).getByText(row.description)).toBeInTheDocument();
  }
});

it("goes to spans with g then p and sessions with g then s", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  await user.keyboard("gp");
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/spans");
  });
  await user.keyboard("gs");
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/sessions");
  });
});

it("ignores shortcuts while typing in the search box", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces");
  const search = await screen.findByRole("searchbox", { name: "Search" });
  await user.click(search);
  await user.keyboard("gs?");
  expect(search).toHaveValue("gs?");
  expect(router.state.location.pathname).toBe("/acme/support-bot/traces");
  expect(screen.queryByRole("dialog", { name: "Keyboard shortcuts" })).not.toBeInTheDocument();
});

it("finds a project settings page by a keyword", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  const dialog = await openPalette(user);
  await user.type(within(dialog).getByRole("combobox"), "retention");
  await user.click(
    await within(dialog).findByRole("option", { name: "Project settings: General" }),
  );
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/settings/general");
  });
});

it("opens an organization settings page from inside a project", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces");
  await screen.findByRole("grid", { name: "Traces" });
  const dialog = await openPalette(user);
  await user.type(within(dialog).getByRole("combobox"), "usage");
  await user.click(
    await within(dialog).findByRole("option", { name: "Organization settings: Plan" }),
  );
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/settings/plan");
  });
});
