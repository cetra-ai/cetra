import { screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { afterEach, beforeEach, expect, it } from "vitest";
import { setExternalNavigator } from "@/lib/external-navigation";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";

const DAY_MS = 24 * 60 * 60 * 1000;

let urls: string[] = [];
let forms: { url: string; fields: Record<string, string> }[] = [];

beforeEach(() => {
  urls = [];
  forms = [];
  setExternalNavigator({
    navigate: (url) => {
      urls.push(url);
    },
    replace: (url) => {
      urls.push(url);
    },
    submit: (url, fields) => {
      forms.push({ url, fields });
    },
  });
});

afterEach(() => {
  setExternalNavigator(undefined);
});

/** A `201 CreatedPersonalToken`; every test that triggers the POST answers this shape. */
function tokenCreated(name: string): Response {
  return HttpResponse.json(
    {
      token: {
        id: "01900000-0000-7000-8000-000000000203",
        name,
        prefix: "pat-cetra-CLITOK",
        scopes: ["read", "write"],
        created_at: "2026-01-05T00:00:00.000000000Z",
      },
      secret: "pat-cetra-CLITOKEN0CLITOKEN0CLITOKEN0CLIT",
    },
    { status: 201 },
  );
}

it("asks the signed-in user to authorize", async () => {
  await renderApp("/cli-auth?port=54321&state=s1&host=laptop");

  expect(
    await screen.findByRole("heading", { name: "Authorize the cetra CLI", level: 1 }),
  ).toBeInTheDocument();
  expect(screen.getByText("ada@example.com")).toBeInTheDocument();
  expect(screen.getByText("CLI on laptop")).toBeInTheDocument();
});

it("creates a token that expires in 90 days and posts it to the CLI", async () => {
  let body: { name: string; scopes: string[]; expires_at: string } | undefined;
  server.use(
    http.post("/api/v1/me/tokens", async ({ request }) => {
      body = (await request.json()) as typeof body;
      return tokenCreated("CLI on laptop");
    }),
  );

  const before = Date.now();
  const { user, queryClient } = await renderApp("/cli-auth?port=54321&state=s1&host=laptop");
  await user.click(await screen.findByRole("button", { name: "Authorize" }));

  await waitFor(() => expect(body).toBeDefined());
  expect(body).toMatchObject({ name: "CLI on laptop", scopes: ["read", "write"] });
  const expiresAt = Date.parse(body?.expires_at ?? "");
  expect(expiresAt).toBeGreaterThanOrEqual(before + 90 * DAY_MS);
  expect(expiresAt).toBeLessThanOrEqual(Date.now() + 90 * DAY_MS);

  // The secret travels in a form POST body, never in a navigated URL.
  await waitFor(() =>
    expect(forms).toEqual([
      {
        url: "http://127.0.0.1:54321/callback",
        fields: { token: "pat-cetra-CLITOKEN0CLITOKEN0CLITOKEN0CLIT", state: "s1" },
      },
    ]),
  );
  expect(urls).toEqual([]);
  expect(
    await screen.findByRole("heading", { name: "You're signed in", level: 1 }),
  ).toBeInTheDocument();
  expect(screen.queryByText(/pat-cetra-CLITOKEN/)).toBeNull();
  await waitFor(() =>
    expect(
      JSON.stringify(
        queryClient
          .getMutationCache()
          .getAll()
          .map((mutation) => mutation.state.data ?? null),
      ),
    ).not.toContain("pat-cetra-CLITOKEN0"),
  );
});

it("names the token CLI without a host", async () => {
  let body: unknown;
  server.use(
    http.post("/api/v1/me/tokens", async ({ request }) => {
      body = await request.json();
      return tokenCreated("CLI");
    }),
  );

  const { user } = await renderApp("/cli-auth?port=54321&state=s1");
  expect(await screen.findByText("not reported")).toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: "Authorize" }));
  await waitFor(() => expect(body).toMatchObject({ name: "CLI" }));
});

it("reads the host from the raw query string", async () => {
  let body: unknown;
  server.use(
    http.post("/api/v1/me/tokens", async ({ request }) => {
      body = await request.json();
      return tokenCreated("CLI on 1e3");
    }),
  );

  // `host=1e3` would arrive as `1000` through the router's JSON parsing of search
  // values; only the raw query string keeps it byte for byte.
  const { user } = await renderApp("/cli-auth?port=54321&state=s1&host=1e3");
  expect(
    await screen.findByRole("heading", { name: "Authorize the cetra CLI", level: 1 }),
  ).toBeInTheDocument();
  expect(screen.getByText("CLI on 1e3")).toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: "Authorize" }));
  await waitFor(() =>
    expect(body).toMatchObject({ name: "CLI on 1e3", scopes: ["read", "write"] }),
  );
});

it("cancels without creating a token", async () => {
  const calls: unknown[] = [];
  server.use(
    http.post("/api/v1/me/tokens", async ({ request }) => {
      calls.push(await request.json());
      return tokenCreated("CLI");
    }),
  );

  const { user } = await renderApp("/cli-auth?port=54321&state=s1");
  await user.click(await screen.findByRole("button", { name: "Cancel" }));

  expect(urls).toEqual(["http://127.0.0.1:54321/callback?error=access_denied&state=s1"]);
  expect(forms).toEqual([]);
  expect(calls).toEqual([]);
  expect(
    await screen.findByRole("heading", { name: "Sign-in cancelled", level: 1 }),
  ).toBeInTheDocument();
});

it("refuses an invalid link", async () => {
  const calls: number[] = [];
  server.use(
    http.post("/api/v1/me/tokens", () => {
      calls.push(1);
      return new HttpResponse(null, { status: 500 });
    }),
  );

  for (const url of [
    "/cli-auth?port=80&state=s1",
    "/cli-auth?port=54321",
    "/cli-auth?port=01024&state=s1",
    "/cli-auth?port=54321&state=s1&host=bad%2Fhost",
  ]) {
    const view = await renderApp(url);
    expect(
      await screen.findByRole("heading", { name: "This sign-in link is invalid", level: 1 }),
    ).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Authorize" })).toBeNull();
    expect(urls).toEqual([]);
    expect(calls).toEqual([]);
    view.unmount();
  }
});

it("accepts the edge ports", async () => {
  const first = await renderApp("/cli-auth?port=1024&state=s1");
  expect(
    await screen.findByRole("heading", { name: "Authorize the cetra CLI", level: 1 }),
  ).toBeInTheDocument();
  first.unmount();

  await renderApp("/cli-auth?port=65535&state=s1");
  expect(
    await screen.findByRole("heading", { name: "Authorize the cetra CLI", level: 1 }),
  ).toBeInTheDocument();
});

it("refuses an all-digit state", async () => {
  const calls: number[] = [];
  server.use(
    http.post("/api/v1/me/tokens", () => {
      calls.push(1);
      return new HttpResponse(null, { status: 500 });
    }),
  );

  await renderApp("/cli-auth?port=54321&state=12345678901234567890");

  expect(
    await screen.findByRole("heading", { name: "This sign-in link is invalid", level: 1 }),
  ).toBeInTheDocument();
  expect(calls).toEqual([]);
  expect(urls).toEqual([]);
});

it("returns a long state with digits byte for byte", async () => {
  server.use(http.post("/api/v1/me/tokens", () => tokenCreated("CLI")));

  const { user } = await renderApp("/cli-auth?port=54321&state=s_12345678901234567890");
  await user.click(await screen.findByRole("button", { name: "Authorize" }));

  await waitFor(() =>
    expect(forms).toEqual([
      {
        url: "http://127.0.0.1:54321/callback",
        fields: {
          token: "pat-cetra-CLITOKEN0CLITOKEN0CLITOKEN0CLIT",
          state: "s_12345678901234567890",
        },
      },
    ]),
  );
});

it("warns before authorizing and shows the host as reported by the CLI", async () => {
  await renderApp("/cli-auth?port=54321&state=s1&host=build-server");

  expect(await screen.findByRole("note")).toHaveTextContent(
    "Only continue if you just ran cetra login on this computer.",
  );
  expect(screen.getByText("Computer name reported by the CLI:")).toHaveTextContent(
    "Computer name reported by the CLI: build-server",
  );
  expect(screen.getByText("It expires", { exact: false })).toHaveTextContent(
    "It expires in 90 days.",
  );
});

it("shows a failure and stays on the page", async () => {
  server.use(
    http.post("/api/v1/me/tokens", () =>
      problemResponse(429, "rate_limited", { detail: "slow down" }),
    ),
  );

  const { user } = await renderApp("/cli-auth?port=54321&state=s1");
  await user.click(await screen.findByRole("button", { name: "Authorize" }));

  const alert = await screen.findByRole("alert");
  expect(alert).toHaveTextContent("slow down");
  expect(urls).toEqual([]);
  expect(screen.getByRole("button", { name: "Authorize" })).toBeEnabled();
});

it("sends a signed-out user through login and keeps the link", async () => {
  server.use(http.get("/api/auth/me", () => problemResponse(401, "unauthorized")));

  const { router } = await renderApp("/cli-auth?port=54321&state=s1");

  await waitFor(() => expect(router.state.location.pathname).toBe("/login"));
  expect(router.state.location.search).toMatchObject({
    redirect: "/cli-auth?port=54321&state=s1",
  });
});
