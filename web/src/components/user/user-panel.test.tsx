import { screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";
import userFixture from "../../../../fixtures/api/responses/users/user-42.json";

it("shows the user's id, an Open page link and the metadata", async () => {
  await renderApp("/acme/support-bot/users?user=user-42");
  const panel = await screen.findByRole("region", { name: "User" });
  expect(within(panel).getByText("user-42")).toBeInTheDocument();
  expect(within(panel).getByRole("link", { name: "Open page" })).toHaveAttribute(
    "href",
    "/acme/support-bot/users/user-42",
  );
  await waitFor(() => {
    const header = panel.querySelector("header") as HTMLElement;
    expect(within(header).getByText("Ada Example")).toBeInTheDocument();
  });
  expect(within(panel).getByText("name")).toBeInTheDocument();
  expect(within(panel).getByText("Recent sessions")).toBeInTheDocument();
});

it("removes the user from the URL when Close is clicked", async () => {
  const { router, user } = await renderApp("/acme/support-bot/users?user=user-42");
  const panel = await screen.findByRole("region", { name: "User" });
  await user.click(within(panel).getByRole("button", { name: "Close" }));
  await waitFor(() => {
    expect(router.state.location.search.user).toBeUndefined();
  });
});

it("shows the name with the id after it, the plan pill, the chips and the activity chart", async () => {
  await renderApp("/acme/support-bot/users?user=user-42");
  const panel = await screen.findByRole("region", { name: "User" });
  await waitFor(() => {
    expect(within(panel).getByRole("img", { name: "Activity" })).toBeInTheDocument();
  });
  const header = panel.querySelector("header");
  expect(header).not.toBeNull();
  expect(within(header as HTMLElement).getByText("Ada Example")).toBeInTheDocument();
  expect(within(header as HTMLElement).getByText("user-42")).toBeInTheDocument();
  expect(within(header as HTMLElement).getByText("pro")).toBeInTheDocument();
  expect(within(panel).getByText("1 session")).toBeInTheDocument();
  expect(within(panel).getByText("3 traces")).toBeInTheDocument();
});

it("links a recent session with its first message, and marks a session with errors", async () => {
  const sessions = userFixture.sessions.map((session) => ({
    ...session,
    first_input_preview: "My order 1234 hasn't arrived.",
    error_trace_count: 1,
  }));
  server.use(
    http.get("/api/v1/projects/:project/users/:user_id", () =>
      HttpResponse.json({ ...userFixture, sessions }),
    ),
  );
  await renderApp("/acme/support-bot/users?user=user-42");
  const panel = await screen.findByRole("region", { name: "User" });
  const link = await within(panel).findByRole("link", { name: /My order 1234/ });
  expect(link).toHaveAttribute("href", "/acme/support-bot/sessions/sess-001");
  expect(within(link).getByText("(has errors)", { exact: false })).toBeInTheDocument();
  expect(link).toHaveAccessibleName(/^My order 1234 hasn't arrived\.\s*\(has errors\)/);
  expect(link).toHaveClass("border-status-error");
});

it("shows the session id when a session has no first message and no errors", async () => {
  await renderApp("/acme/support-bot/users?user=user-42");
  const panel = await screen.findByRole("region", { name: "User" });
  const link = await within(panel).findByRole("link", { name: /sess-001/ });
  expect(link).toHaveAttribute("href", "/acme/support-bot/sessions/sess-001");
  expect(within(link).queryByText("(has errors)", { exact: false })).not.toBeInTheDocument();
  expect(link).toHaveAccessibleName(/^sess-001/);
});

it("says the user has no traces when the detail is not found, not 'No sessions'", async () => {
  server.use(
    http.get("/api/v1/projects/:project/users/:user_id", () =>
      HttpResponse.json({ status: 404, title: "Not found", code: "not_found" }, { status: 404 }),
    ),
  );
  await renderApp("/acme/support-bot/users?user=user-42");
  const panel = await screen.findByRole("region", { name: "User" });
  expect(await within(panel).findByText("No traces from this user")).toBeInTheDocument();
  expect(within(panel).queryByText("No sessions")).not.toBeInTheDocument();
});

it("lists the latest sessions even when the range has none", async () => {
  server.use(
    http.get("/api/v1/projects/:project/users/:user_id", ({ request }) => {
      if (new URL(request.url).searchParams.has("from")) {
        return HttpResponse.json({
          ...userFixture,
          user: { ...userFixture.user, session_count: 0 },
          sessions: [],
        });
      }
      return HttpResponse.json(userFixture);
    }),
  );
  await renderApp("/acme/support-bot/users?user=user-42");
  const panel = await screen.findByRole("region", { name: "User" });
  const link = await within(panel).findByRole("link", { name: /sess-001/ });
  expect(link).toHaveAttribute("href", "/acme/support-bot/sessions/sess-001");
  expect(within(panel).queryByText("No sessions")).not.toBeInTheDocument();
});

it("shows at most 10 recent sessions and links to all of the user's sessions", async () => {
  const sessions = Array.from({ length: 12 }, (_, index) => ({
    ...userFixture.sessions[0],
    session_id: `sess-a${String(index + 1).padStart(2, "0")}`,
  }));
  server.use(
    http.get("/api/v1/projects/:project/users/:user_id", () =>
      HttpResponse.json({
        ...userFixture,
        user: { ...userFixture.user, session_count: 12 },
        sessions,
      }),
    ),
  );
  await renderApp("/acme/support-bot/users?user=user-42");
  const panel = await screen.findByRole("region", { name: "User" });
  const sessionLinks = await within(panel).findAllByRole("link", {
    name: /sess-a\d\d/,
  });
  expect(sessionLinks).toHaveLength(10);
  const all = within(panel).getByRole("link", { name: /All sessions/ });
  expect(all.getAttribute("href")).toContain("/acme/support-bot/users/user-42");
  expect(all.getAttribute("href")).toContain("tab=sessions");
});

it("returns to the user panel from a session opened there", async () => {
  const { router, user } = await renderApp("/acme/support-bot/users?user=user-42");
  const panel = await screen.findByRole("region", { name: "User" });
  await user.click(await within(panel).findByRole("link", { name: /sess-001/ }));
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/sessions/sess-001");
  });

  // Selecting a turn keeps the way back: the session page's own navigation must not drop it.
  const conversation = await screen.findByRole("region", { name: "Conversation" });
  const turns = within(conversation).getAllByRole("button", { name: /support-agent/ });
  const secondTurn = turns[1];
  if (!secondTurn) {
    throw new Error("missing second turn");
  }
  await user.click(secondTurn);
  await waitFor(() => {
    expect(router.state.location.search).toMatchObject({ trace: SECOND_TRACE });
  });
  await user.click(await screen.findByRole("link", { name: "Back to Ada Example" }));
  await waitFor(() => {
    expect(router.state.location.pathname).toBe("/acme/support-bot/users");
    expect(router.state.location.search.user).toBe("user-42");
  });
  await waitFor(() => {
    expect(router.state.status).toBe("idle");
  });
});

const SECOND_TRACE = "a1000000000000000000000000000002";
