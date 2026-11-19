import { useQuery } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { Loader2 } from "lucide-react";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { adminSettingsQuery } from "@/api/queries-admin";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { FormSuccess } from "@/components/form/form-success";
import { SettingsCard } from "@/components/settings/settings-card";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { LicenseCard } from "@/ee/license-card";

export const Route = createFileRoute("/admin/settings")({
  component: AdminSettingsPage,
});

function AdminSettingsPage() {
  const { data: me } = $api.useSuspenseQuery("get", "/api/auth/me");
  const settings = useQuery(adminSettingsQuery);
  const test = $api.useMutation("post", "/api/v1/admin/settings/test-email");
  const [sent, setSent] = useState<string | undefined>();
  const [testError, setTestError] = useState<string | undefined>();

  if (settings.isPending) {
    return (
      <div className="flex flex-col gap-2">
        <Skeleton className="h-6 w-full" />
        <Skeleton className="h-6 w-full" />
        <Skeleton className="h-6 w-full" />
        <Skeleton className="h-6 w-full" />
        <Skeleton className="h-6 w-full" />
        <Skeleton className="h-6 w-full" />
      </div>
    );
  }
  if (settings.error) {
    throw settings.error;
  }
  const data = settings.data;

  function send() {
    setSent(undefined);
    setTestError(undefined);
    test.mutate(
      {},
      {
        onSuccess: () => setSent(`Test email sent to ${me.user.email}.`),
        onError: (error) => setTestError(testEmailError(error)),
      },
    );
  }

  return (
    <div className="flex flex-col gap-6">
      <p className="text-body text-muted-foreground">
        Read-only. Change them in cetra.toml or CETRA_* variables, then restart the server.
      </p>
      <SettingsCard title="Instance settings">
        <dl
          aria-label="Instance settings"
          className="grid grid-cols-[minmax(10rem,max-content)_1fr] gap-x-6 gap-y-2 text-body"
        >
          <dt className="text-muted-foreground">Public URL</dt>
          <dd className="font-mono">{data.public_url}</dd>
          <dt className="text-muted-foreground">Sign-up</dt>
          <dd>{signupLabel(data.signup)}</dd>
          <dt className="text-muted-foreground">Sign-up domains</dt>
          <dd>{data.signup_domains.length > 0 ? data.signup_domains.join(", ") : "Any domain"}</dd>
          <dt className="text-muted-foreground">Password sign-in</dt>
          <dd>{data.password_enabled ? "Enabled" : "Instance admins only"}</dd>
          <dt className="text-muted-foreground">Email verification</dt>
          <dd>{data.email_verification_required ? "Required" : "Not required"}</dd>
          <dt className="text-muted-foreground">Single sign-on required for</dt>
          <dd>
            {data.enforce_sso_domains.length > 0
              ? data.enforce_sso_domains.join(", ")
              : "No domains"}
            {data.enforce_sso_domains.length > 0 && !data.enforce_sso_applied
              ? " — not enforced: needs an enterprise licence"
              : null}
          </dd>
          <dt className="text-muted-foreground">Email (SMTP)</dt>
          <dd>{data.smtp_enabled ? "Configured" : "Not configured"}</dd>
          {data.email_from ? (
            <>
              <dt className="text-muted-foreground">From address</dt>
              <dd>{data.email_from}</dd>
            </>
          ) : null}
          <dt className="text-muted-foreground">Sign-in providers</dt>
          <dd>
            {data.providers.length > 0
              ? data.providers
                  .map((provider) => `${provider.display_name} (${provider.id})`)
                  .join(", ")
              : "None"}
          </dd>
          <dt className="text-muted-foreground">Trusted proxies</dt>
          <dd className="font-mono">
            {data.trusted_proxies.length > 0 ? data.trusted_proxies.join(", ") : "None"}
          </dd>
        </dl>
      </SettingsCard>
      <LicenseCard />
      <SettingsCard title="Email" description={`Sends a test message to ${me.user.email}.`}>
        <div className="flex flex-col gap-3">
          <DisabledReason
            reason={
              data.smtp_enabled
                ? undefined
                : "Email is not configured. Set CETRA_SMTP_URL and restart the server."
            }
          >
            <Button
              type="button"
              variant="outline"
              className="w-fit"
              disabled={!data.smtp_enabled || test.isPending}
              onClick={send}
            >
              {test.isPending ? <Loader2 className="animate-spin" aria-hidden="true" /> : null}
              Send test email
            </Button>
          </DisabledReason>
          <FormSuccess message={sent} />
          <FormError message={testError} />
        </div>
      </SettingsCard>
    </div>
  );
}

function signupLabel(policy: Schema<"SignupPolicy">): string {
  switch (policy) {
    case "open":
      return "Open to anyone";
    case "invite_only":
      return "Invite only";
    case "disabled":
      return "Disabled";
  }
}

function testEmailError(error: unknown): string {
  const status = problemOf(error)?.status;
  if (status === 503) {
    return "The SMTP server could not be reached or refused the message.";
  }
  if (status === 400) {
    return "Email is not configured.";
  }
  if (status === 429) {
    return "Too many requests. Wait a minute and try again.";
  }
  return problemMessage(error, "The test email could not be sent.");
}
