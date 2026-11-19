import { useQueryClient, useSuspenseQuery } from "@tanstack/react-query";
import { createFileRoute, Link, redirect, useNavigate } from "@tanstack/react-router";
import { type FormEvent, useEffect, useRef, useState } from "react";
import { authConfigQuery, clearUserData } from "@/api/auth";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { meQuery } from "@/api/queries";
import { AuthLayout } from "@/components/auth/auth-layout";
import { AUTH_LINK_CLASS } from "@/components/auth/link-class";
import {
  loginErrorMessage,
  NO_SIGN_IN_METHOD,
  oauthErrorMessage,
  RATE_LIMITED,
} from "@/components/auth/messages";
import { OrDivider, ProviderButtons } from "@/components/auth/provider-buttons";
import { FormError } from "@/components/form/form-error";
import { FormSuccess } from "@/components/form/form-success";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { Button } from "@/components/ui/button";
import { navigateExternal, replaceExternal } from "@/lib/external-navigation";
import { onlyProvider } from "@/lib/only-provider";
import { authorizeHref, ssoAuthorizeHref } from "@/lib/redirect";
import { parseLoginSearch } from "@/lib/search/auth";

export const Route = createFileRoute("/login")({
  validateSearch: (search) => parseLoginSearch(search),
  beforeLoad: async ({ context, search }) => {
    const config = await context.queryClient.ensureQueryData(authConfigQuery);
    if (config.setup_required) {
      throw redirect({ to: "/setup" });
    }
    // staleTime 0: a cached `me` from before a sign-out or a 401 must not count.
    const me = await context.queryClient
      .fetchQuery({ ...meQuery, staleTime: 0 })
      .catch(() => undefined);
    if (me) {
      throw redirect({ href: search.redirect ?? "/" });
    }
  },
  component: LoginPage,
});

/** The text after the last `@`, lowercased (D-P-60). */
function domainOf(email: string): string {
  return email.slice(email.lastIndexOf("@") + 1).toLowerCase();
}

function LoginPage() {
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const search = Route.useSearch();
  const { data: config } = useSuspenseQuery(authConfigQuery);
  // Email first when the server routes SSO domains and this is not the instance
  // admins' `?password=1` path (D-P-60).
  const emailFirst = config.sso_discovery && !search.password;
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
  // Only read while `emailFirst`, so it may start at "email" on every page load.
  const [step, setStep] = useState<"email" | "password">("email");
  const [redirecting, setRedirecting] = useState(false);
  const [email, setEmail] = useState(search.email ?? "");
  const [password, setPassword] = useState("");
  const [clientErrors, setClientErrors] = useState<{ email?: string; password?: string }>({});
  const [resendDone, setResendDone] = useState(false);
  // gcTime 0: the request carries the password, so its variables must leave the
  // mutation cache as soon as the page unmounts (README § Pitfalls).
  const login = $api.useMutation("post", "/api/auth/login", { gcTime: 0 });
  const resend = $api.useMutation("post", "/api/auth/email/verify/resend");
  const discover = $api.useMutation("post", "/api/auth/sso/discover");
  // `replace`: Back from the provider must not land here and leave again (D-S-5). The ref
  // keeps a re-run of the effect from starting a second login.
  const started = useRef(false);
  useEffect(() => {
    if (startNow && startHref !== undefined && !started.current) {
      started.current = true;
      replaceExternal(startHref);
    }
  }, [startNow, startHref]);
  const serverFieldErrors = fieldErrors(login.error);
  const needsVerification = problemOf(login.error)?.code === "email_not_verified";

  function onDiscover(event: FormEvent) {
    event.preventDefault();
    if (email.trim().length === 0) {
      setClientErrors({ email: "Enter your email address." });
      return;
    }
    setClientErrors({});
    const address = email.trim();
    discover.mutate(
      { body: { email: address } },
      {
        onSuccess: ({ sso }) => {
          if (sso) {
            setRedirecting(true);
            navigateExternal(ssoAuthorizeHref(domainOf(address), search.redirect));
            return;
          }
          setStep("password");
        },
      },
    );
  }

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    login.reset();
    const errors: { email?: string; password?: string } = {};
    if (email.trim().length === 0) {
      errors.email = "Enter your email address.";
    }
    if (password.length === 0) {
      errors.password = "Enter your password.";
    }
    setClientErrors(errors);
    if (errors.email || errors.password) {
      return;
    }
    setResendDone(false);
    login.mutate(
      { body: { email: email.trim(), password } },
      {
        onSuccess: (me) => {
          clearUserData(queryClient);
          queryClient.setQueryData(meQuery.queryKey, me);
          void navigate({ href: search.redirect ?? "/" });
        },
      },
    );
  }

  if (startNow && only !== undefined) {
    return (
      <AuthLayout title="Sign in" description={`Redirecting to ${only.display_name}…`}>
        <ProviderButtons providers={[only]} redirect={search.redirect} />
      </AuthLayout>
    );
  }

  return (
    <AuthLayout
      title="Sign in"
      description={
        !config.password_enabled && search.password
          ? "Password sign-in is for instance admins."
          : undefined
      }
      footer={
        config.password_enabled || config.signup === "open" ? (
          <>
            {config.password_enabled ? (
              <p>
                <Link to="/reset" className={AUTH_LINK_CLASS}>
                  Forgot password?
                </Link>
              </p>
            ) : null}
            {config.signup === "open" ? (
              <p>
                Don't have an account?{" "}
                <Link to="/signup" className={AUTH_LINK_CLASS}>
                  Create an account
                </Link>
              </p>
            ) : null}
          </>
        ) : undefined
      }
    >
      <FormError message={search.error ? oauthErrorMessage(search.error) : undefined} />
      <FormSuccess message={search.signed_out ? "You're signed out." : undefined} />
      {emailFirst && step === "email" ? (
        <form noValidate onSubmit={onDiscover} className="flex flex-col gap-4">
          <TextField
            id="email"
            label="Email"
            type="email"
            autoComplete="email"
            required
            value={email}
            onChange={setEmail}
            disabled={discover.isPending || redirecting}
            error={clientErrors.email}
          />
          <FormError
            message={
              discover.error
                ? problemOf(discover.error)?.status === 429
                  ? RATE_LIMITED
                  : "Sign-in failed. Try again."
                : undefined
            }
          />
          <SubmitButton pending={discover.isPending || redirecting}>Continue</SubmitButton>
        </form>
      ) : (
        <>
          <ProviderButtons providers={config.providers} redirect={search.redirect} />
          {config.providers.length > 0 && showPassword ? <OrDivider /> : null}
          {showPassword ? (
            <form noValidate onSubmit={onSubmit} className="flex flex-col gap-4">
              <TextField
                id="email"
                label="Email"
                type="email"
                autoComplete="email"
                required
                value={email}
                onChange={setEmail}
                disabled={login.isPending}
                error={clientErrors.email ?? serverFieldErrors.email}
              />
              <TextField
                id="password"
                label="Password"
                type="password"
                autoComplete="current-password"
                required
                value={password}
                onChange={setPassword}
                disabled={login.isPending}
                error={clientErrors.password}
              />
              <FormError message={login.error ? loginErrorMessage(login.error) : undefined} />
              {needsVerification && config.smtp_enabled ? (
                <>
                  <Button
                    variant="outline"
                    type="button"
                    disabled={resend.isPending}
                    onClick={() => {
                      resend.mutate(
                        { body: { email: email.trim() } },
                        { onSuccess: () => setResendDone(true) },
                      );
                    }}
                  >
                    Resend verification email
                  </Button>
                  <FormSuccess
                    message={
                      resendDone
                        ? "If this address needs verification, we sent a new link."
                        : undefined
                    }
                  />
                  <FormError
                    message={
                      resend.error
                        ? problemOf(resend.error)?.status === 429
                          ? RATE_LIMITED
                          : problemMessage(resend.error, "The email could not be sent. Try again.")
                        : undefined
                    }
                  />
                </>
              ) : null}
              <SubmitButton pending={login.isPending}>Sign in</SubmitButton>
            </form>
          ) : null}
          {!showPassword && config.providers.length === 0 ? (
            <p className="text-body">{NO_SIGN_IN_METHOD}</p>
          ) : null}
          {!config.password_enabled && !search.password && config.providers.length > 0 ? (
            <Link
              to="/login"
              search={{ password: true, redirect: search.redirect }}
              className={AUTH_LINK_CLASS}
            >
              Instance admin? Sign in with a password
            </Link>
          ) : null}
          {emailFirst ? (
            <Button
              variant="link"
              type="button"
              className="self-start px-0"
              onClick={() => {
                setStep("email");
                login.reset();
                setClientErrors({});
              }}
            >
              Use another email
            </Button>
          ) : null}
        </>
      )}
    </AuthLayout>
  );
}
