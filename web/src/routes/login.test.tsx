import { screen, waitFor } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { afterEach, expect, it } from "vitest";
import { NO_SIGN_IN_METHOD, oauthErrorMessage, RATE_LIMITED } from "@/components/auth/messages";
import { setExternalNavigator } from "@/lib/external-navigation";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import authConfig from "../../../fixtures/api/static/auth-config.json";
import meFixture from "../../../fixtures/api/static/me.json";

function signedOut() {
  server.use(http.get("/api/auth/me", () => problemResponse(401, "unauthorized")));
}

function withConfig(overrides: Partial<typeof authConfig>) {
  server.use(
    http.get("/api/auth/config", () => HttpResponse.json({ ...authConfig, ...overrides })),
  );
}

function loginSucceeds() {
  const bodies: unknown[] = [];
  server.use(
    http.post("/api/auth/login", async ({ request }) => {
      bodies.push(await request.json());
      return HttpResponse.json(meFixture);
    }),
  );
  return bodies;
}

function loginAnswers(response: () => Response) {
  server.use(http.post("/api/auth/login", response));
}

async function fillAndSubmit(user: UserEvent, email: string, password: string) {
  await user.type(screen.getByLabelText("Email"), email);
  await user.type(screen.getByLabelText("Password"), password);
  await user.click(screen.getByRole("button", { name: "Sign in" }));
}

type AppRouter = Awaited<ReturnType<typeof renderApp>>["router"];

// Waits for the navigation to finish loading, not just to start: a load still running when the
// file's environment is torn down rejects outside any test.
async function settlesAt(router: AppRouter, pathname: string) {
  await waitFor(() => {
    expect(router.state.status).toBe("idle");
    expect(router.state.resolvedLocation?.pathname).toBe(pathname);
  });
}

const CLOUD = { id: "oidc:cloud", display_name: "cetra Cloud" };

function onlyCloud(overrides: Partial<typeof authConfig> = {}) {
  withConfig({ password_enabled: false, providers: [CLOUD], ...overrides });
}

function recordNavigation() {
  const replaced: string[] = [];
  const navigated: string[] = [];
  setExternalNavigator({
    navigate: (url) => {
      navigated.push(url);
    },
    replace: (url) => {
      replaced.push(url);
    },
    submit: () => {
      throw new Error("the login page never posts a form");
    },
  });
  return { replaced, navigated };
}

it("signs in and returns to the redirect target", async () => {
  signedOut();
  const bodies = loginSucceeds();
  const { router, user, queryClient } = await renderApp(
    "/login?redirect=%2Facme%2Fsupport-bot%2Ftraces",
  );
  await fillAndSubmit(user, "ada@example.com", "correct horse battery");
  await settlesAt(router, "/acme/support-bot/traces");
  expect(bodies).toEqual([{ email: "ada@example.com", password: "correct horse battery" }]);
  // gcTime 0: once the page has unmounted, TanStack drops the mutation, whose variables
  // hold the password and whose data holds the signed-in user.
  const cached = () =>
    JSON.stringify(
      queryClient
        .getMutationCache()
        .getAll()
        .map((mutation) => ({
          data: mutation.state.data ?? null,
          variables: mutation.state.variables ?? null,
        })),
    );
  await waitFor(() => {
    expect(cached()).not.toContain("correct horse battery");
    expect(cached()).not.toContain("ada@example.com");
  });
});

it("goes home after signing in without a redirect", async () => {
  signedOut();
  loginSucceeds();
  const { router, user } = await renderApp("/login");
  await fillAndSubmit(user, "ada@example.com", "correct horse battery");
  await settlesAt(router, "/acme");
});

it("sends the user to setup when the server needs it", async () => {
  signedOut();
  withConfig({ setup_required: true });
  const { router } = await renderApp("/login");
  await settlesAt(router, "/setup");
  expect(screen.getByText(/Open the setup link printed in the server log/)).toBeInTheDocument();
});

it("ignores an unsafe redirect", async () => {
  signedOut();
  loginSucceeds();
  const { router, user } = await renderApp("/login?redirect=%2F%2Fevil.example");
  expect(screen.getByRole("link", { name: "Continue with GitHub" })).toHaveAttribute(
    "href",
    "/api/auth/authorize/github",
  );
  await fillAndSubmit(user, "ada@example.com", "correct horse battery");
  // The app stays on its origin and lands on the default page, not on the given redirect.
  await settlesAt(router, "/acme");
  expect(router.state.location.href).toBe("/acme");
});

it("sends a user who is already signed in to the redirect target", async () => {
  const { router } = await renderApp("/login?redirect=%2Facme%2Fsupport-bot%2Ftraces");
  expect(router.state.location.pathname).toBe("/acme/support-bot/traces");
  expect(screen.queryByRole("heading", { name: "Sign in" })).toBeNull();
});

it("shows a button per identity provider", async () => {
  signedOut();
  await renderApp("/login?redirect=%2Facme");
  expect(screen.getByRole("link", { name: "Continue with GitHub" })).toHaveAttribute(
    "href",
    "/api/auth/authorize/github?redirect=%2Facme",
  );
  expect(screen.getByRole("link", { name: "Continue with Okta" })).toHaveAttribute(
    "href",
    "/api/auth/authorize/oidc%3Aokta?redirect=%2Facme",
  );
});

it("hides the password form when password sign-in is disabled", async () => {
  signedOut();
  withConfig({ password_enabled: false });
  await renderApp("/login");
  expect(screen.queryByLabelText("Email")).toBeNull();
  expect(screen.queryByRole("button", { name: "Sign in" })).toBeNull();
  expect(screen.getByRole("link", { name: "Continue with GitHub" })).toBeInTheDocument();
});

it("says when no sign-in method is configured", async () => {
  signedOut();
  withConfig({ password_enabled: false, providers: [] });
  await renderApp("/login");
  expect(screen.getByText(NO_SIGN_IN_METHOD)).toBeInTheDocument();
});

it("requires an email and a password", async () => {
  signedOut();
  let calls = 0;
  server.use(
    http.post("/api/auth/login", () => {
      calls += 1;
      return HttpResponse.json(meFixture);
    }),
  );
  const { user } = await renderApp("/login");
  await user.click(screen.getByRole("button", { name: "Sign in" }));
  expect(screen.getByText("Enter your email address.")).toBeInTheDocument();
  expect(screen.getByText("Enter your password.")).toBeInTheDocument();
  expect(screen.getByLabelText("Email")).toHaveAttribute("aria-invalid", "true");
  expect(calls).toBe(0);
});

it("explains a wrong password", async () => {
  signedOut();
  loginAnswers(() => problemResponse(401, "unauthorized"));
  const { router, user } = await renderApp("/login");
  await fillAndSubmit(user, "ada@example.com", "wrong password");
  expect(await screen.findByRole("alert")).toHaveTextContent("Incorrect email or password.");
  expect(router.state.location.pathname).toBe("/login");
});

it("explains a domain that must use single sign-on", async () => {
  signedOut();
  loginAnswers(() => problemResponse(403, "sso_required"));
  const { user } = await renderApp("/login");
  await fillAndSubmit(user, "ada@example.com", "correct horse battery");
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Your organization requires single sign-on. Enter your email again to continue.",
  );
});

it("resends the verification email", async () => {
  signedOut();
  withConfig({ smtp_enabled: true });
  loginAnswers(() => problemResponse(403, "email_not_verified"));
  const bodies: unknown[] = [];
  server.use(
    http.post("/api/auth/email/verify/resend", async ({ request }) => {
      bodies.push(await request.json());
      return new HttpResponse(null, { status: 204 });
    }),
  );
  const { user } = await renderApp("/login");
  await fillAndSubmit(user, "ada@example.com", "correct horse battery");
  await user.click(await screen.findByRole("button", { name: "Resend verification email" }));
  expect(bodies).toEqual([{ email: "ada@example.com" }]);
  expect(
    await screen.findByText("If this address needs verification, we sent a new link."),
  ).toBeInTheDocument();
});

it("offers no resend without email", async () => {
  signedOut();
  loginAnswers(() => problemResponse(403, "email_not_verified"));
  const { user } = await renderApp("/login");
  await fillAndSubmit(user, "ada@example.com", "correct horse battery");
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Verify your email address before signing in.",
  );
  expect(screen.queryByRole("button", { name: "Resend verification email" })).toBeNull();
});

it("explains rate limiting", async () => {
  signedOut();
  loginAnswers(() => problemResponse(429, "rate_limited"));
  const { user } = await renderApp("/login");
  await fillAndSubmit(user, "ada@example.com", "correct horse battery");
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Too many attempts. Wait a minute and try again.",
  );
});

it("shows the identity provider error from the URL", async () => {
  signedOut();
  await renderApp("/login?error=signup_closed");
  expect(await screen.findByRole("alert")).toHaveTextContent(oauthErrorMessage("signup_closed"));
});

it("prefills the email from the URL", async () => {
  signedOut();
  await renderApp("/login?email=bob%40example.com");
  expect(screen.getByLabelText("Email")).toHaveValue("bob@example.com");
});

it("submits with Enter", async () => {
  signedOut();
  const bodies = loginSucceeds();
  const { router, user } = await renderApp("/login");
  await user.type(screen.getByLabelText("Email"), "ada@example.com");
  await user.type(screen.getByLabelText("Password"), "correct horse battery{Enter}");
  expect(bodies).toHaveLength(1);
  await settlesAt(router, "/acme");
});

it("explains a rate-limited resend", async () => {
  signedOut();
  withConfig({ smtp_enabled: true });
  loginAnswers(() => problemResponse(403, "email_not_verified"));
  server.use(
    http.post("/api/auth/email/verify/resend", () => problemResponse(429, "rate_limited")),
  );
  const { user } = await renderApp("/login");
  await fillAndSubmit(user, "ada@example.com", "correct horse battery");
  await user.click(await screen.findByRole("button", { name: "Resend verification email" }));
  expect(
    await screen.findByText("Too many attempts. Wait a minute and try again."),
  ).toBeInTheDocument();
  expect(screen.queryByText("If this address needs verification, we sent a new link.")).toBeNull();
});

it("explains a failed resend", async () => {
  signedOut();
  withConfig({ smtp_enabled: true });
  loginAnswers(() => problemResponse(403, "email_not_verified"));
  server.use(http.post("/api/auth/email/verify/resend", () => problemResponse(500, "internal")));
  const { user } = await renderApp("/login");
  await fillAndSubmit(user, "ada@example.com", "correct horse battery");
  await user.click(await screen.findByRole("button", { name: "Resend verification email" }));
  expect(await screen.findByText("Internal Server Error")).toBeInTheDocument();
});

it("explains a resend that never reached the server", async () => {
  signedOut();
  withConfig({ smtp_enabled: true });
  loginAnswers(() => problemResponse(403, "email_not_verified"));
  server.use(http.post("/api/auth/email/verify/resend", () => HttpResponse.error()));
  const { user } = await renderApp("/login");
  await fillAndSubmit(user, "ada@example.com", "correct horse battery");
  await user.click(await screen.findByRole("button", { name: "Resend verification email" }));
  expect(await screen.findByText("The email could not be sent. Try again.")).toBeInTheDocument();
});

it("shows the divider between buttons and a form", async () => {
  signedOut();
  await renderApp("/login");
  expect(screen.getByText("or")).toBeInTheDocument();
});

it("hides the divider without providers", async () => {
  signedOut();
  withConfig({ providers: [] });
  await renderApp("/login");
  expect(screen.queryByText("or")).toBeNull();
});

it("hides the divider without the password form", async () => {
  signedOut();
  withConfig({ password_enabled: false });
  await renderApp("/login");
  expect(screen.queryByText("or")).toBeNull();
});

it("explains disabled password sign-in", async () => {
  signedOut();
  loginAnswers(() => problemResponse(403, "forbidden"));
  const { user } = await renderApp("/login");
  await fillAndSubmit(user, "ada@example.com", "correct horse battery");
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Password sign-in is disabled on this server.",
  );
});

it("shows a failed origin check as it is", async () => {
  signedOut();
  loginAnswers(() => problemResponse(403, "forbidden", { detail: "origin check failed" }));
  const { user } = await renderApp("/login");
  await fillAndSubmit(user, "ada@example.com", "correct horse battery");
  expect(await screen.findByRole("alert")).toHaveTextContent("origin check failed");
});

it("clears a stale server error when the client check fails", async () => {
  signedOut();
  loginAnswers(() => problemResponse(401, "unauthorized"));
  const { user } = await renderApp("/login");
  await fillAndSubmit(user, "ada@example.com", "correct horse battery");
  expect(await screen.findByRole("alert")).toHaveTextContent("Incorrect email or password.");
  await user.clear(screen.getByLabelText("Password"));
  await user.click(screen.getByRole("button", { name: "Sign in" }));
  expect(screen.getByText("Enter your password.")).toBeInTheDocument();
  expect(screen.queryByText("Incorrect email or password.")).toBeNull();
});

it("links to sign-up when sign-up is open", async () => {
  signedOut();
  withConfig({ signup: "open" });
  await renderApp("/login");
  expect(await screen.findByRole("link", { name: "Create an account" })).toHaveAttribute(
    "href",
    "/signup",
  );
});

it("hides the sign-up link unless sign-up is open", async () => {
  signedOut();
  await renderApp("/login");
  await screen.findByRole("heading", { name: "Sign in" });
  expect(screen.queryByRole("link", { name: "Create an account" })).toBeNull();
});

it("links to the reset page when password sign-in is on", async () => {
  signedOut();
  await renderApp("/login");
  expect(await screen.findByRole("link", { name: "Forgot password?" })).toHaveAttribute(
    "href",
    "/reset",
  );
});

it("hides the reset link when password sign-in is off", async () => {
  signedOut();
  withConfig({ password_enabled: false });
  await renderApp("/login");
  await screen.findByRole("heading", { name: "Sign in" });
  expect(screen.queryByRole("link", { name: "Forgot password?" })).toBeNull();
});

afterEach(() => {
  setExternalNavigator(undefined);
});

function discoverAnswers(response: () => Response) {
  const bodies: unknown[] = [];
  server.use(
    http.post("/api/auth/sso/discover", async ({ request }) => {
      bodies.push(await request.json());
      return response();
    }),
  );
  return bodies;
}

it("sends an SSO email to its org's identity provider", async () => {
  signedOut();
  withConfig({ sso_discovery: true });
  const bodies = discoverAnswers(() => HttpResponse.json({ sso: true }));
  const navigated: string[] = [];
  setExternalNavigator({
    navigate: (url) => {
      navigated.push(url);
    },
    replace: () => {
      throw new Error("the SSO login never replaces the page");
    },
    submit: () => {
      throw new Error("the SSO login never posts a form");
    },
  });
  const { user } = await renderApp("/login?redirect=%2Facme");
  await user.type(await screen.findByLabelText("Email"), "ada@acme.com");
  await user.click(screen.getByRole("button", { name: "Continue" }));
  await waitFor(() =>
    expect(navigated).toEqual(["/api/auth/sso/authorize?domain=acme.com&redirect=%2Facme"]),
  );
  expect(bodies).toEqual([{ email: "ada@acme.com" }]);
  expect(screen.queryByLabelText("Password")).toBeNull();
});

it("shows the password form for another email and goes back on request", async () => {
  signedOut();
  withConfig({ sso_discovery: true });
  discoverAnswers(() => HttpResponse.json({ sso: false }));
  const bodies = loginSucceeds();
  const { router, user } = await renderApp("/login");
  await user.type(await screen.findByLabelText("Email"), "ada@example.com");
  await user.click(screen.getByRole("button", { name: "Continue" }));
  await user.type(await screen.findByLabelText("Password"), "correct horse battery");
  expect(screen.getByLabelText("Email")).toHaveValue("ada@example.com");
  await user.click(screen.getByRole("button", { name: "Sign in" }));
  await settlesAt(router, "/acme");
  expect(bodies).toEqual([{ email: "ada@example.com", password: "correct horse battery" }]);
});

it("returns from the password step to the email step", async () => {
  signedOut();
  withConfig({ sso_discovery: true });
  discoverAnswers(() => HttpResponse.json({ sso: false }));
  const { user } = await renderApp("/login");
  await user.type(await screen.findByLabelText("Email"), "ada@example.com");
  await user.click(screen.getByRole("button", { name: "Continue" }));
  await user.click(await screen.findByRole("button", { name: "Use another email" }));
  expect(screen.queryByLabelText("Password")).toBeNull();
  expect(screen.getByLabelText("Email")).toHaveValue("ada@example.com");
});

it("skips the email step with ?password=1", async () => {
  signedOut();
  withConfig({ sso_discovery: true });
  const bodies = discoverAnswers(() => HttpResponse.json({ sso: true }));
  await renderApp("/login?password=1");
  expect(await screen.findByLabelText("Password")).toBeInTheDocument();
  expect(bodies).toEqual([]);
});

it("explains a rate-limited email check", async () => {
  signedOut();
  withConfig({ sso_discovery: true });
  discoverAnswers(() => problemResponse(429, "rate_limited"));
  const { user } = await renderApp("/login");
  await user.type(await screen.findByLabelText("Email"), "ada@example.com");
  await user.click(screen.getByRole("button", { name: "Continue" }));
  expect(await screen.findByRole("alert")).toHaveTextContent(RATE_LIMITED);
});

it("asks for an email before checking it", async () => {
  signedOut();
  withConfig({ sso_discovery: true });
  const bodies = discoverAnswers(() => HttpResponse.json({ sso: false }));
  const { user } = await renderApp("/login");
  await user.click(await screen.findByRole("button", { name: "Continue" }));
  expect(screen.getByText("Enter your email address.")).toBeInTheDocument();
  expect(bodies).toEqual([]);
});

it.each(["seat_limit", "sso_required", "sso_unavailable"] as const)(
  "explains the %s error from the URL",
  async (code) => {
    signedOut();
    withConfig({ sso_discovery: true });
    await renderApp(`/login?error=${code}`);
    expect(await screen.findByRole("alert")).toHaveTextContent(oauthErrorMessage(code));
  },
);

it("starts the only provider at once, keeping the redirect", async () => {
  signedOut();
  onlyCloud();
  const { replaced, navigated } = recordNavigation();
  const { router } = await renderApp("/login?redirect=%2Facme%2Fsupport-bot%2Ftraces");
  expect(await screen.findByText("Redirecting to cetra Cloud…")).toBeInTheDocument();
  await waitFor(() =>
    expect(replaced).toEqual([
      "/api/auth/authorize/oidc%3Acloud?redirect=%2Facme%2Fsupport-bot%2Ftraces",
    ]),
  );
  expect(navigated).toEqual([]);
  expect(screen.getByRole("link", { name: "Continue with cetra Cloud" })).toBeInTheDocument();
  expect(screen.queryByLabelText("Email")).toBeNull();
  expect(router.state.location.pathname).toBe("/login");
});

it("shows the instance admins' password form on password=1", async () => {
  signedOut();
  onlyCloud();
  const { replaced } = recordNavigation();
  const bodies = loginSucceeds();
  const { router, user } = await renderApp("/login?password=1");
  expect(await screen.findByText("Password sign-in is for instance admins.")).toBeInTheDocument();
  await fillAndSubmit(user, "root@example.com", "correct horse battery");
  await waitFor(() =>
    expect(bodies).toEqual([{ email: "root@example.com", password: "correct horse battery" }]),
  );
  expect(replaced).toEqual([]);
  expect(screen.queryByRole("link", { name: "Forgot password?" })).toBeNull();
  await settlesAt(router, "/acme");
});

it("shows a provider error instead of starting the provider again", async () => {
  signedOut();
  onlyCloud();
  const { replaced } = recordNavigation();
  await renderApp("/login?error=signup_closed");
  expect(await screen.findByRole("alert")).toHaveTextContent(oauthErrorMessage("signup_closed"));
  expect(screen.getByRole("link", { name: "Continue with cetra Cloud" })).toBeInTheDocument();
  expect(replaced).toEqual([]);
});

it("does not start the provider right after signing out", async () => {
  signedOut();
  onlyCloud();
  const { replaced } = recordNavigation();
  await renderApp("/login?signed_out=1");
  expect(await screen.findByRole("status")).toHaveTextContent("You're signed out.");
  expect(screen.getByRole("link", { name: "Continue with cetra Cloud" })).toBeInTheDocument();
  expect(replaced).toEqual([]);
});

it("links instance admins to the password form", async () => {
  signedOut();
  onlyCloud();
  const { replaced } = recordNavigation();
  const { router, user } = await renderApp("/login?error=provider_error");
  await user.click(
    await screen.findByRole("link", { name: "Instance admin? Sign in with a password" }),
  );
  expect(await screen.findByLabelText("Password")).toBeInTheDocument();
  await waitFor(() => {
    expect(router.state.status).toBe("idle");
    expect(router.state.resolvedLocation?.search).toEqual({ password: true });
  });
  expect(replaced).toEqual([]);
});

it("shows the password form on password=1 under org SSO", async () => {
  signedOut();
  onlyCloud({ sso_discovery: true });
  const { replaced } = recordNavigation();
  await renderApp("/login?password=1");
  expect(await screen.findByLabelText("Password")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Continue" })).toBeNull();
  expect(replaced).toEqual([]);
});

it("keeps the buttons with two providers", async () => {
  signedOut();
  withConfig({ password_enabled: false });
  const { replaced } = recordNavigation();
  await renderApp("/login");
  expect(await screen.findByRole("link", { name: "Continue with GitHub" })).toBeInTheDocument();
  expect(screen.getByRole("link", { name: "Continue with Okta" })).toBeInTheDocument();
  expect(
    await screen.findByRole("link", { name: "Instance admin? Sign in with a password" }),
  ).toBeInTheDocument();
  expect(replaced).toEqual([]);
});

it("keeps the email step under org SSO", async () => {
  signedOut();
  onlyCloud({ sso_discovery: true });
  const { replaced } = recordNavigation();
  await renderApp("/login");
  expect(await screen.findByRole("button", { name: "Continue" })).toBeInTheDocument();
  expect(screen.getByLabelText("Email")).toBeInTheDocument();
  expect(replaced).toEqual([]);
});

it("keeps the password form when password sign-in is on", async () => {
  signedOut();
  withConfig({ providers: [CLOUD] });
  const { replaced } = recordNavigation();
  await renderApp("/login");
  expect(await screen.findByLabelText("Password")).toBeInTheDocument();
  expect(
    screen.queryByRole("link", { name: "Instance admin? Sign in with a password" }),
  ).toBeNull();
  expect(replaced).toEqual([]);
});
