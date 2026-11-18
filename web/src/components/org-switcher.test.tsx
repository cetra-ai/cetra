import { screen, waitFor } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { authConfigQuery } from "@/api/auth";
import type { Schema } from "@/api/types";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import authConfig from "../../../fixtures/api/static/auth-config.json";
import meFixture from "../../../fixtures/api/static/me.json";

async function openDialog(user: UserEvent) {
  await user.click(screen.getByRole("button", { name: "Acme" }));
  await user.click(screen.getByRole("menuitem", { name: "New organization" }));
  await screen.findByRole("dialog", { name: "New organization" });
  // The dialog's scroll lock keeps `pointer-events: none` on body; clicks inside
  // it work once the menu that opened it is gone.
  await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
}

it("creates an organization and opens it", async () => {
  const orgs = [...meFixture.orgs];
  let body: unknown;
  server.use(
    http.get("/api/auth/me", () => HttpResponse.json({ user: meFixture.user, orgs })),
    http.post("/api/v1/orgs", async ({ request }) => {
      body = await request.json();
      const created = {
        id: "01900000-0000-7000-8000-000000000011",
        slug: "beta",
        name: "Beta",
        role: "owner",
        demo: false,
      };
      orgs.push(created);
      return HttpResponse.json(created, { status: 201 });
    }),
    http.get("/api/v1/orgs/beta/projects", () =>
      HttpResponse.json({ data: [], next_cursor: null }),
    ),
  );

  const { router, user } = await renderApp("/acme");
  await openDialog(user);
  await user.type(screen.getByLabelText("Name"), "Beta");
  await user.click(screen.getByRole("button", { name: "Create organization" }));

  expect(body).toEqual({ name: "Beta" });
  await waitFor(() => expect(router.state.location.pathname).toBe("/beta"));
  expect(await screen.findByRole("heading", { name: "Beta" })).toBeInTheDocument();
});

it("lists the demo org last, badged", async () => {
  const demo = {
    id: "01900000-0000-7000-8000-000000000020",
    slug: "cetra-demo",
    name: "cetra Demo",
    role: "viewer",
    demo: true,
  };
  server.use(
    http.get("/api/auth/me", () =>
      HttpResponse.json({ user: meFixture.user, orgs: [demo, ...meFixture.orgs] }),
    ),
  );

  const { user } = await renderApp("/acme");
  await user.click(screen.getByRole("button", { name: "Acme" }));
  const items = screen.getAllByRole("menuitemradio");
  expect(items.map((item) => item.textContent)).toEqual(["Acme", "cetra DemoDemo"]);
  expect(items[1]).toHaveTextContent("Demo");
});

it("links to the create URL when users may not create orgs", async () => {
  server.use(
    http.get("/api/auth/config", () =>
      HttpResponse.json({
        ...authConfig,
        org_create_allowed: false,
        org_create_url: "https://cloud.example.com/new",
      }),
    ),
  );

  const { user } = await renderApp("/acme");
  await user.click(screen.getByRole("button", { name: "Acme" }));
  // The item is offered while the config loads, then becomes the link.
  await waitFor(() =>
    expect(screen.getByRole("menuitem", { name: "New organization" })).toHaveAttribute(
      "href",
      "https://cloud.example.com/new",
    ),
  );
  expect(screen.queryByRole("dialog", { name: "New organization" })).toBeNull();
});

it("hides the new-organization item when users may not create orgs and no URL is set", async () => {
  server.use(
    http.get("/api/auth/config", () =>
      HttpResponse.json({ ...authConfig, org_create_allowed: false }),
    ),
  );

  const { queryClient, user } = await renderApp("/acme");
  await waitFor(() =>
    expect(
      queryClient.getQueryData<Schema<"AuthConfig">>(authConfigQuery.queryKey)?.org_create_allowed,
    ).toBe(false),
  );
  await user.click(screen.getByRole("button", { name: "Acme" }));
  await screen.findByRole("menuitemradio", { name: "Acme" });
  expect(screen.queryByRole("menuitem", { name: "New organization" })).toBeNull();
});

it("sends the slug and shows a taken slug", async () => {
  let body: unknown;
  server.use(
    http.post("/api/v1/orgs", async ({ request }) => {
      body = await request.json();
      return problemResponse(409, "conflict", { detail: "slug taken" });
    }),
  );

  const { user } = await renderApp("/acme");
  await openDialog(user);
  await user.type(screen.getByLabelText("Name"), "Acme two");
  await user.type(screen.getByLabelText("Slug (optional)"), "acme");
  await user.click(screen.getByRole("button", { name: "Create organization" }));

  expect(body).toEqual({ name: "Acme two", slug: "acme" });
  expect(await screen.findByText("This slug is taken.")).toBeInTheDocument();
  expect(screen.getByRole("dialog", { name: "New organization" })).toBeInTheDocument();
});

it("explains a reserved slug", async () => {
  server.use(
    http.post("/api/v1/orgs", () =>
      problemResponse(400, "bad_request", {
        errors: [{ field: "slug", message: "reserved slug" }],
      }),
    ),
  );

  const { user } = await renderApp("/acme");
  await openDialog(user);
  await user.type(screen.getByLabelText("Name"), "Beta");
  await user.click(screen.getByRole("button", { name: "Create organization" }));

  expect(await screen.findByText("This slug is reserved. Choose another.")).toBeInTheDocument();
});

it("shows a generic failure and starts clean when reopened", async () => {
  server.use(http.post("/api/v1/orgs", () => problemResponse(500, "internal")));

  const { user } = await renderApp("/acme");
  await openDialog(user);
  await user.type(screen.getByLabelText("Name"), "Beta");
  await user.click(screen.getByRole("button", { name: "Create organization" }));

  expect(await screen.findByRole("alert")).toHaveTextContent("Internal Server Error");
  await user.click(screen.getByRole("button", { name: "Cancel" }));
  expect(screen.queryByRole("dialog")).toBeNull();

  await openDialog(user);
  expect(screen.getByLabelText("Name")).toHaveValue("");
  expect(screen.queryByRole("alert")).toBeNull();
});

it("asks for a name without calling the server", async () => {
  const { user } = await renderApp("/acme");
  await openDialog(user);
  await user.click(screen.getByRole("button", { name: "Create organization" }));

  expect(await screen.findByText("Enter a name.")).toBeInTheDocument();
  expect(screen.getByLabelText("Name")).toHaveAttribute("aria-invalid", "true");
});

it("shows the server's name error under the field", async () => {
  server.use(
    http.post("/api/v1/orgs", () =>
      problemResponse(400, "bad_request", {
        errors: [{ field: "name", message: "must be 1 to 100 characters" }],
      }),
    ),
  );

  const { user } = await renderApp("/acme");
  await openDialog(user);
  await user.type(screen.getByLabelText("Name"), "Beta");
  await user.click(screen.getByRole("button", { name: "Create organization" }));

  expect(await screen.findByText("must be 1 to 100 characters")).toBeInTheDocument();
  expect(screen.getByLabelText("Name")).toHaveAttribute("aria-invalid", "true");
});
