# S4 — The invite page starts the only provider (R7)

**Commit subject:** `web: start the only sign-in provider from the invite page`

**Goal:** under the same condition as the login page (`onlyProvider`, D-S-5), the invite page leaves at once for the provider when the visitor has no session, with `redirect` back to `/invite/<token>`, so an invitee without an account signs up through the provider and comes back to click "Join <org>" (D-S-7). It does not when sign-up is `disabled` and the invited email has no account, and not after the visitor signed out of another account on this page (D-S-6). S5 closes the track out.

## Read first

- README § Decisions D-S-5, D-S-6, D-S-7.
- S3's `web/src/lib/only-provider.ts`, `replaceExternal` in `web/src/lib/external-navigation.ts`, and the `recordNavigation` helper of `web/src/routes/login.test.tsx`.
- `web/src/routes/invite/$token.tsx` and `web/src/routes/invite/$token.test.tsx`, all of them.
- `fixtures/api/static/invite-preview.json` (`email` is `guest@example.com`, `account_exists` is `false`) and `fixtures/api/static/me.json` (the user is `ada@example.com`).
- `docs/spec/11-ui.md` § Signed out (as S3 left it) and the invite page's text in § Pages, if it has one (`grep -n "invite" docs/spec/11-ui.md`).

**Check first:** `web/src/lib/only-provider.ts` exists and exports `onlyProvider`, and `external-navigation.ts` exports `replaceExternal`. If not, S3 is missing: stop and report.

## Files

| File | Change |
|---|---|
| `web/src/routes/invite/$token.tsx` | start at once; no restart after "Sign out" |
| `web/src/routes/invite/$token.test.tsx` | tests |
| `docs/spec/11-ui.md` | the invite page under R7 |
| `CHANGELOG.md` | extend S3's `Changed` line |

## Steps

### 1. `InvitePage` (`routes/invite/$token.tsx`)

1. Imports: `useEffect, useRef` from `react` (with `type FormEvent, type ReactNode, useState`); `import { replaceExternal } from "@/lib/external-navigation";`, `import { onlyProvider } from "@/lib/only-provider";`, `import { authorizeHref } from "@/lib/redirect";`.

2. In `InvitePage`, after `const back = \`/invite/${token}\`;`, add (all hooks stay above the first early `return`):

   ```ts
     // Set once the visitor signs out of another account here: the provider would sign them
     // straight back in, so the page shows its button instead (D-S-6).
     const [signedOutHere, setSignedOutHere] = useState(false);
     const signedOut = problemOf(me.error)?.status === 401;
     const only = config.data === undefined ? undefined : onlyProvider(config.data);
     // R7 (D-S-7): a visitor without a session goes straight to the only provider and back
     // here, unless sign-up is disabled and the invited email has no account.
     const closed = config.data?.signup === "disabled" && preview.data?.account_exists === false;
     const startNow =
       only !== undefined && preview.data !== undefined && signedOut && !closed && !signedOutHere;
     const started = useRef(false);
     useEffect(() => {
       if (startNow && only !== undefined && !started.current) {
         started.current = true;
         replaceExternal(authorizeHref(only.id, { redirect: back }));
       }
     }, [startNow, only, back]);
   ```

3. Delete the later line `const signedOut = problemOf(me.error)?.status === 401;` (now defined above).

4. Right after the `if (checkEmail) { … }` block, add:

   ```tsx
     if (startNow && only !== undefined) {
       return (
         <AuthLayout
           title={`Join ${preview.data.org.name}`}
           description={`Redirecting to ${only.display_name}…`}
         >
           <ProviderButtons providers={[only]} redirect={back} />
         </AuthLayout>
       );
     }
   ```

   (`startNow` implies `preview.data` is defined; if TypeScript does not narrow it, the earlier `preview.isPending`/`isError` returns already guarantee it here, so write `preview.data.org.name` as the code below this point already does.)

5. Give `SignedInAsOther` a prop `onSignedOut: () => void` (add it to its props type) and call `onSignedOut();` first thing in its logout `onSuccess`, before `queryClient.removeQueries(…)`. Pass `onSignedOut={() => setSignedOutHere(true)}` where `InvitePage` renders `<SignedInAsOther …/>`.

6. When `signedOutHere && only !== undefined`, the visitor sees the normal signed-out content (`SignInToAccept` or `SignupToJoin`, whose provider buttons carry `redirect={back}`). In the final `return` of `InvitePage`, replace the child `{children}` of `<AuthLayout title={`Join ${preview.data.org.name}`} …>` with:

   ```tsx
         <>
           {signedOutHere && signedOut && only !== undefined ? (
             <p className="text-sm text-muted-foreground">
               {`You're signed out here. If ${only.display_name} signs you back in as the other account, sign out there first.`}
             </p>
           ) : null}
           {children}
         </>
   ```

   The notice shows only while the visitor has no session (the `SignInToAccept` and `SignupToJoin` branches).

### 2. Tests (`routes/invite/$token.test.tsx`)

Add imports `afterEach` (from `vitest`) and `setExternalNavigator` (from `@/lib/external-navigation`), `afterEach(() => { setExternalNavigator(undefined); });`, and the helpers:

```ts
const CLOUD = { id: "oidc:cloud", display_name: "cetra Cloud" };
const START = authorizeHref("oidc:cloud", { redirect: URL_PATH });

function onlyCloud(overrides: Partial<typeof authConfig> = {}) {
  withConfig({ password_enabled: false, providers: [CLOUD], ...overrides });
}

function recordReplace() {
  const replaced: string[] = [];
  setExternalNavigator({
    navigate: () => {
      throw new Error("the invite page never navigates with a history entry");
    },
    replace: (url) => {
      replaced.push(url);
    },
    submit: () => {
      throw new Error("the invite page never posts a form");
    },
  });
  return replaced;
}
```

(import `authorizeHref` from `@/lib/redirect`; it builds the same URL the page does.)

1. `starts the only provider for a visitor without an account`: `signedOut(); onlyCloud(); const replaced = recordReplace();` render `URL_PATH`; `await waitFor(() => expect(replaced).toEqual([START]))`; the text `Redirecting to cetra Cloud…` and the heading `Join Acme` are shown.
2. `starts the only provider for a visitor with an account`: as 1 with the preview overridden to `account_exists: true` (as `asks an existing user to sign in` does).
3. `keeps the sign-up-disabled message`: `signedOut(); onlyCloud({ signup: "disabled" });` → the text `/^Sign-up is disabled on this server/` is shown and `replaced` is empty.
4. `starts the provider for an existing account even when sign-up is disabled`: `signedOut(); onlyCloud({ signup: "disabled" });` with `account_exists: true` → `replaced` equals `[START]`.
5. `does not start the provider for a signed-in invitee`: `onlyCloud()`, `/api/auth/me` answers the invited user (`{ user: GUEST, orgs: [] }`, the `signedInAsGuest` helper) → the "Join Acme" button shows, `replaced` is empty.
6. `does not start the provider again after signing out of another account`: the setup of `asks a different signed-in user to sign out` plus `onlyCloud()` and `const replaced = recordReplace();`; after clicking "Sign out", the link "Continue with cetra Cloud" shows (find it with `findByRole`), the text `/^You're signed out here\./` is shown, and `replaced` is empty.
7. `keeps the page with password sign-in on`: `signedOut(); withConfig({ providers: [CLOUD] }); const replaced = recordReplace();` → the "Create account and join" button shows, `replaced` is empty.

Existing tests keep passing: the default config has password sign-in on and two providers. After each render, wait for something on the page before asserting that `replaced` is empty.

### 3. Spec 11

In § "**Signed out (D-L-1).**", after the sentence S3 added about `/login` leaving at once, add: `Under the same condition, the invite page sends a visitor without a session to the provider with \`redirect\` back to the invite, so an invitee without an account signs up there and comes back to click "Join"; not when sign-up is \`disabled\` and the email has no account, and not after the visitor signed out of another account on that page (R7, D-S-7).`

### 4. CHANGELOG

Extend S3's `### Changed` line `- Web UI: with password sign-in off and exactly one sign-in provider, the login page goes straight to that provider; …` so its first sentence reads `…, the login page and, for a visitor without a session, the invite page go straight to that provider;` (keep the rest).

## Check

```bash
just check-web
just check
```

## Done when

- The seven tests pass with every existing web test; `just check` is green.
- One commit with the subject above.
