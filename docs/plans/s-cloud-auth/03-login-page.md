# S3 — The login page starts the only provider; the admins' password form (R7, R13)

**Commit subject:** `web: start the only sign-in provider from the login page`

**Goal:** with password login off, no org SSO discovery and exactly one provider, `/login` leaves at once for that provider, keeping `?redirect=` (D-S-5). It does not on `?password=1` (which now shows the password form for instance admins even when password login is off, D-S-4), on `?error=`, or on `?signed_out=1`, where sign-out now lands (D-S-6). The reset page lets an instance admin use a reset link, the account page shows the password section to instance admins, and the admin settings page says "Instance admins only".

## Read first

- README § Decisions D-S-4, D-S-5, D-S-6, D-S-8.
- `web/src/routes/login.tsx` and `web/src/routes/login.test.tsx`, all of them.
- `web/src/lib/external-navigation.ts` and `web/src/lib/external-navigation.test.ts`.
- `web/src/lib/search/auth.ts` and `web/src/lib/search/auth.test.ts`.
- `web/src/lib/redirect.ts` (`authorizeHref`).
- `web/src/components/auth/provider-buttons.tsx`, `web/src/components/auth/auth-layout.tsx`, `web/src/components/form/form-success.tsx`, `web/src/components/auth/link-class.ts`.
- `web/src/components/auth/use-sign-out.ts` and `web/src/components/user-menu.test.tsx`.
- `web/src/routes/reset.tsx` and `web/src/routes/reset.test.tsx`.
- `web/src/routes/account.tsx` and `web/src/routes/account.test.tsx`.
- `web/src/routes/admin/settings.tsx` and `web/src/routes/admin/settings.test.tsx`.
- `web/src/routes/cli-auth.test.tsx` (its `setExternalNavigator` call).
- `web/README.md` § Pitfalls and § Conventions (search params), if present.
- `fixtures/api/static/auth-config.json` (two providers, password on).
- `docs/spec/11-ui.md` § Signed out and § Login with org SSO.

**Check first:** `LoginSearch` in `web/src/lib/search/auth.ts` has `password?: boolean;`, and `login.tsx` has `const emailFirst = config.sso_discovery && !search.password;`. If not, stop and report.

## Files

| File | Change |
|---|---|
| `web/src/lib/external-navigation.ts` | `replace` in `ExternalNavigator`; `replaceExternal` |
| `web/src/lib/external-navigation.test.ts` | a test of `replaceExternal` |
| `web/src/test/auth-gate.test.tsx` | the sign-out test's search assertion |
| `web/src/routes/cli-auth.test.tsx` | its test navigator gains `replace` |
| `web/src/lib/search/auth.ts`, `auth.test.ts` | `signed_out` flag |
| `web/src/lib/only-provider.ts` (new), `web/src/lib/only-provider.test.ts` (new) | `onlyProvider` |
| `web/src/routes/login.tsx`, `login.test.tsx` | start at once; the admins' form; signed-out note; admin link |
| `web/src/components/auth/use-sign-out.ts`, `web/src/components/user-menu.test.tsx` | sign-out lands on `/login?signed_out=1` |
| `web/src/routes/reset.tsx`, `reset.test.tsx` | a reset link works with password login off |
| `web/src/routes/account.tsx`, `account.test.tsx` | the password section for instance admins |
| `web/src/routes/admin/settings.tsx`, `settings.test.tsx` | "Instance admins only" |
| `docs/spec/11-ui.md` | § Signed out, § Login with org SSO |
| `CHANGELOG.md` | `Changed` line |

## Steps

### 1. `replaceExternal` (`lib/external-navigation.ts`)

1. Add to the type `ExternalNavigator`, after `navigate`:

   ```ts
     /** Leaves the app for a URL without keeping the current page in history. */
     replace: (url: string) => void;
   ```

2. Add to `browserNavigator`, after `navigate`:

   ```ts
     replace: (url) => {
       window.location.replace(url);
     },
   ```

3. Add after `navigateExternal`:

   ```ts
   /** Leaves the app for `url`, replacing the current history entry, so Back does not return to it. */
   export function replaceExternal(url: string): void {
     current.replace(url);
   }
   ```

4. In `lib/external-navigation.test.ts`, add `replaceExternal goes through the navigator's replace`: `setExternalNavigator` with `navigate`, `replace` and `submit` that record their URLs, call `replaceExternal("/api/auth/authorize/github")`, assert `replace` got exactly that URL and `navigate`/`submit` nothing, then `setExternalNavigator(undefined)` (import `afterEach` or restore at the end). `browserNavigator.replace` itself stays untested: jsdom does not implement `location.replace`.
5. In `web/src/routes/cli-auth.test.tsx`, the object passed to `setExternalNavigator` in `beforeEach` gains `replace: (url) => { urls.push(url); },` after `navigate`. In `login.test.tsx`, the existing SSO test's navigator gains `replace: () => { throw new Error("the SSO login never replaces the page"); },`.

### 2. `signed_out` (`lib/search/auth.ts`)

1. In `LoginSearch`, after `password?: boolean;`, add:

   ```ts
     /** `?signed_out=1`: the user just signed out; never start a provider at once (D-S-6). */
     signed_out?: boolean;
   ```

2. Add above `parseLoginSearch`:

   ```ts
   /** `1` (as the URL gives it) or `true` (the router re-validating its own output). */
   function parseFlag(value: unknown): true | undefined {
     return value === "1" || value === 1 || value === true ? true : undefined;
   }
   ```

3. In `parseLoginSearch`, replace the `password:` entry and its comment with:

   ```ts
       // The router validates its own output again, so `true` is read back as well.
       password: parseFlag(search.password),
       signed_out: parseFlag(search.signed_out),
   ```

4. In `auth.test.ts`: the two `Object.keys(result).sort()` lists become `["email", "error", "password", "redirect", "signed_out"]`; the first test's `toEqual` stays as it is (`toEqual` ignores keys whose value is `undefined`). Add a test `reads the signed-out flag from 1 and only from 1` mirroring the password one (`"1"`, `1`, `true` → `true`; `"true"`, missing → `undefined`).

### 3. `onlyProvider` (new `lib/only-provider.ts`)

```ts
import type { Schema } from "@/api/types";

/**
 * The provider the login and invite pages start at once (R7, D-S-5): password sign-in is
 * off, no org provider routes a domain (`sso_discovery`, D-P-60), and exactly one instance
 * provider is configured. `undefined` otherwise.
 */
export function onlyProvider(config: Schema<"AuthConfig">): Schema<"AuthProvider"> | undefined {
  if (config.password_enabled || config.sso_discovery || config.providers.length !== 1) {
    return undefined;
  }
  return config.providers[0];
}
```

`lib/only-provider.test.ts` (import the fixture as `login.test.tsx` does, `../../../fixtures/api/static/auth-config.json`, and build configs with spreads; `const CLOUD = { id: "oidc:cloud", display_name: "llmobs Cloud" };`):
- password off, discovery off, `[CLOUD]` → `CLOUD`;
- password on, `[CLOUD]` → `undefined`;
- password off, discovery on, `[CLOUD]` → `undefined`;
- password off, two providers (the fixture's) → `undefined`;
- password off, no provider → `undefined`.

### 4. The login page (`routes/login.tsx`)

1. Imports: `useEffect, useRef` from `react` (with `type FormEvent, useState`); `replaceExternal` next to `navigateExternal`; `authorizeHref` next to `ssoAuthorizeHref` (both from `@/lib/redirect`); `import { onlyProvider } from "@/lib/only-provider";`.

2. In `LoginPage`, right after `const emailFirst = …;` and before the `useState` calls, add:

   ```ts
     // R7 (D-S-5, D-S-6): with one provider and no password, leave for it at once, except on
     // the instance admins' form, after a failed provider login, and right after signing out.
     const only = onlyProvider(config);
     const startNow =
       only !== undefined && !search.password && search.error === undefined && !search.signed_out;
     const startHref =
       only === undefined ? undefined : authorizeHref(only.id, { redirect: search.redirect });
     // The password form also shows on `?password=1` when password sign-in is off: it is the
     // instance admins' way in (D-S-4).
     const showPassword = config.password_enabled || search.password === true;
   ```

3. After the last hook of `LoginPage` (the `const discover = $api.useMutation(…)` line), add:

   ```ts
     // `replace`: Back from the provider must not land here and leave again (D-S-5). The ref
     // keeps a re-run of the effect from starting a second login.
     const started = useRef(false);
     useEffect(() => {
       if (startNow && startHref !== undefined && !started.current) {
         started.current = true;
         replaceExternal(startHref);
       }
     }, [startNow, startHref]);
   ```

   Then, right before the component's `return (`, add:

   ```tsx
     if (startNow && only !== undefined) {
       return (
         <AuthLayout title="Sign in" description={`Redirecting to ${only.display_name}…`}>
           <ProviderButtons providers={[only]} redirect={search.redirect} />
         </AuthLayout>
       );
     }
   ```

   (No hook may come after this `if`; `onDiscover` and `onSubmit` are plain functions, so they may stay above it.)

4. In the returned `<AuthLayout title="Sign in" …>`, add the prop:

   ```tsx
         description={
           !config.password_enabled && search.password
             ? "Password sign-in is for instance admins."
             : undefined
         }
   ```

5. Right after `<FormError message={search.error ? oauthErrorMessage(search.error) : undefined} />`, add:

   ```tsx
         <FormSuccess message={search.signed_out ? "You're signed out." : undefined} />
   ```

6. In the branch that is not the email step (the fragment that starts with `<ProviderButtons providers={config.providers} …/>`):
   - `{config.providers.length > 0 && config.password_enabled ? <OrDivider /> : null}` → `{config.providers.length > 0 && showPassword ? <OrDivider /> : null}`;
   - `{config.password_enabled ? (` before `<form noValidate onSubmit={onSubmit}` → `{showPassword ? (`;
   - `{!config.password_enabled && config.providers.length === 0 ? (` → `{!showPassword && config.providers.length === 0 ? (`;
   - right after that `NO_SIGN_IN_METHOD` block, add:

     ```tsx
               {!config.password_enabled && !search.password && config.providers.length > 0 ? (
                 <Link
                   to="/login"
                   search={{ password: true, redirect: search.redirect }}
                   className={AUTH_LINK_CLASS}
                 >
                   Instance admin? Sign in with a password
                 </Link>
               ) : null}
     ```

   The footer's "Forgot password?" stays on `config.password_enabled` (forgot is refused for everyone with password login off, D-S-3).

### 5. Sign-out (`components/auth/use-sign-out.ts`)

Replace `void navigate({ to: "/login" })` with `void navigate({ to: "/login", search: { signed_out: true } })`, and the doc comment `/** POST /api/auth/logout, then /login. … */` with `/** POST /api/auth/logout, then /login?signed_out=1, which never starts a provider (D-S-6). A failed logout still leaves the page. */`.

In `web/src/test/auth-gate.test.tsx`, the sign-out test asserts `expect(router.state.location.search).toEqual({});` after `await waitFor(() => expect(router.state.location.pathname).toBe("/login"));`; change it to `expect(router.state.location.search).toEqual({ signed_out: true });` (it still proves no `redirect` is kept).

In `web/src/components/user-menu.test.tsx`, in the test with `await user.click(screen.getByRole("menuitem", { name: "Sign out" }));` that also asserts `expect(signedOut).toBe(true);`, add after the `waitFor(… "/login")` line: `expect(router.state.location.search).toMatchObject({ signed_out: true });`.

### 6. The reset page (`routes/reset.tsx`)

1. In `ResetPage`, the `if (!config.password_enabled) { return (…disabled…) }` block applies only without a token. Change its condition to `if (!config.password_enabled && token === undefined)` and its `title` to the constant `"Reset your password"`. A reset link (from `llmobs admin reset-password` or the admin users page) always shows the form; the server accepts it for instance admins only (D-S-3), and a 403 already shows "Password sign-in is disabled on this server." through `resetFormError`.
2. Pass `passwordEnabled={config.password_enabled}` to `NewPassword`, whose props become `{ token, passwordEnabled }: { token: string; passwordEnabled: boolean }`. In `NewPassword`, add `const loginSearch = passwordEnabled ? {} : { password: true as const };` and give every `<Link to="/login" …>` inside `NewPassword` (the done view's footer link and its "Sign in" button link, the form's footer link, the SSO "Go to sign in" link) the prop `search={loginSearch}`, so an admin is sent to the password form instead of the provider.
3. `reset.test.tsx`: replace `explains when password sign-in is disabled with a token` with `shows the reset form with a token when password sign-in is disabled`: `withConfig({ password_enabled: false })`, `/reset?token=tok123` shows the "New password" field (`{ selector: "input#password" }`); a reset answered 204 (override `http.post("/api/auth/password/reset", () => new HttpResponse(null, { status: 204 }))`) shows "Password updated", and the "Sign in" link's `href` starts with `/login?` and contains `password=` (assert with `expect(link.getAttribute("href")).toMatch(/^\/login\?.*password=/)`: the router decides how it writes the flag); and a reset answered `problemResponse(403, "forbidden")` shows the alert "Password sign-in is disabled on this server.". `explains when password sign-in is disabled` (no token) stays as it is.

### 7. The account page (`routes/account.tsx`)

1. Imports: `useSuspenseQuery` from `@tanstack/react-query`, `authConfigQuery` from `@/api/auth`, `meQuery` from `@/api/queries`. Give the route a loader so the page never suspends on first render (the auth gate only loads `me`; existing account tests assert right after `renderApp`): `createFileRoute("/account")({ loader: ({ context }) => context.queryClient.ensureQueryData(authConfigQuery), component: AccountPage })`, as `reset.tsx` does.
2. In `AccountPage`, add before `return`:

   ```ts
     const { data: config } = useSuspenseQuery(authConfigQuery);
     const { data: me } = useSuspenseQuery(meQuery);
     // With password sign-in off, only instance admins keep a password (D-S-3).
     const showPassword = config.password_enabled || me.user.is_instance_admin;
   ```

3. Render the `<section … aria-labelledby="password-heading">` only when `showPassword`.
4. `account.test.tsx`: add `hides the password section when password sign-in is off` (override `/api/auth/config` with `{ ...authConfig, password_enabled: false }`, and `/api/auth/me` with `{ ...meFixture, user: { ...meFixture.user, is_instance_admin: false } }`; after `await screen.findByRole("heading", { name: "Account", level: 1 })`, the heading "Password" (level 2) is absent) and `keeps the password section for an instance admin when password sign-in is off` (same config, the `me.json` fixture as is, whose user is an instance admin: the heading is present). Import the fixtures as `login.test.tsx` does (`../../../fixtures/api/static/auth-config.json`, `…/me.json`). The existing tests keep passing: the default fixture has password sign-in on.

### 8. Admin settings (`routes/admin/settings.tsx`)

Replace `<dd>{data.password_enabled ? "Enabled" : "Disabled"}</dd>` with `<dd>{data.password_enabled ? "Enabled" : "Instance admins only"}</dd>`. In `settings.test.tsx`, add `labels password sign-in that is off`: `server.use(withSettings({ password_enabled: false }))`, then `settingValue("Password sign-in")` has text `Instance admins only`. If a test already asserts `"Disabled"` for password sign-in, change it.

### 9. Tests in `login.test.tsx`

Add at the top, after the existing helpers:

```ts
const CLOUD = { id: "oidc:cloud", display_name: "llmobs Cloud" };

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
```

(The file already resets the navigator in `afterEach`.) Tests:

1. `starts the only provider at once, keeping the redirect`: `signedOut(); onlyCloud(); const { replaced, navigated } = recordNavigation();` render `/login?redirect=%2Facme%2Fsupport-bot%2Ftraces`; `await waitFor(() => expect(replaced).toEqual(["/api/auth/authorize/oidc%3Acloud?redirect=%2Facme%2Fsupport-bot%2Ftraces"]))`; `navigated` is empty; the text `Redirecting to llmobs Cloud…` is shown; the link "Continue with llmobs Cloud" is there; no "Email" field.
2. `shows the instance admins' password form on password=1`: `signedOut(); onlyCloud(); const { replaced } = recordNavigation(); const bodies = loginSucceeds();` render `/login?password=1`; the text `Password sign-in is for instance admins.` is shown; `await fillAndSubmit(user, "root@example.com", "correct horse battery")`; `await waitFor(() => expect(bodies).toEqual([{ email: "root@example.com", password: "correct horse battery" }]))`; `replaced` is empty; no "Forgot password?" link.
3. `shows a provider error instead of starting the provider again`: `onlyCloud()`, `/login?error=signup_closed`: the alert has `oauthErrorMessage("signup_closed")`; the "Continue with llmobs Cloud" link is there; `replaced` is empty.
4. `does not start the provider right after signing out`: `onlyCloud()`, `/login?signed_out=1`: the status text `You're signed out.`; the provider link; `replaced` is empty.
5. `links instance admins to the password form`: `onlyCloud()`, `/login?error=provider_error`; click the link "Instance admin? Sign in with a password"; `await screen.findByLabelText("Password")`; `await waitFor(() => expect(router.state.location.search).toEqual({ password: true }))` (the link drops `error`); then `expect(replaced).toEqual([])`.
5b. `shows the password form on password=1 under org SSO`: `signedOut(); onlyCloud({ sso_discovery: true }); const { replaced } = recordNavigation();` render `/login?password=1`: the "Password" field shows, the "Continue" button does not, `replaced` is empty (D-P-60 with D-S-4).
6. `keeps the buttons with two providers`: `withConfig({ password_enabled: false })` (two providers), `/login`: both "Continue with" links, `replaced` empty, the admin link is there.
7. `keeps the email step under org SSO`: `onlyCloud({ sso_discovery: true })`, `/login`: the "Continue" button and "Email" field, `replaced` empty.
8. `keeps the password form when password sign-in is on`: `withConfig({ providers: [CLOUD] })`, `/login`: the "Password" field, `replaced` empty, no admin link.

Each test that renders the page calls `signedOut()` first, as the existing tests do. After each `render`, wait for something on the page (`findBy…`) before asserting that `replaced` is empty, so the effect has had its chance to run.

### 10. Spec 11

1. § "**Signed out (D-L-1).**": append: `When password login is off, \`sso_discovery\` is false and exactly one provider is configured, \`/login\` leaves at once for that provider with its \`?redirect=\` (location replaced, so Back does not return to it), except on \`?password=1\`, \`?error=\` and \`?signed_out=1\` (R7, D-S-5). Sign-out lands on \`/login\` with the \`signed_out\` flag (written \`?signed_out=true\`), which says "You're signed out." and never leaves at once, since the provider would sign the user straight back in (D-S-6). With password login off, the page links to \`/login?password=1\` ("Instance admin? Sign in with a password"), which shows the password form for instance admins (D-S-4); the reset page accepts a reset link, and the account page shows the password section, for instance admins only.`
2. § "**Login with org SSO (track P's M5).**": change `/login?password=1\` skips the email step (instance admins, D-P-58, D-P-60).` to `/login?password=1\` skips the email step and shows the password form even when password login is off (instance admins, D-P-58, D-P-60, D-S-4).`

### 11. CHANGELOG

Under `## [Unreleased]` → `### Changed`, add:

```markdown
- Web UI: with password sign-in off and exactly one sign-in provider, the login page goes
  straight to that provider; signing out lands on a page that does not. Instance admins sign in
  with their password at `/login?password=1`, which the login page links to (D-S-4, D-S-5, D-S-6).
```

## Check

```bash
just check-web
just check
```

## Done when

- The new tests pass, existing web tests pass unchanged except those this file names; `just check` is green.
- One commit with the subject above.
