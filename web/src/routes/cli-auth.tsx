import { createFileRoute, useRouter } from "@tanstack/react-router";
import { Loader2 } from "lucide-react";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { AuthLayout } from "@/components/auth/auth-layout";
import { FormError } from "@/components/form/form-error";
import { Button } from "@/components/ui/button";
import {
  cliAuthSearchFrom,
  cliCallbackForm,
  cliCancelUrl,
  cliTokenExpiry,
  cliTokenName,
  parseCliRequest,
} from "@/lib/cli-auth";
import { navigateExternal, submitExternalForm } from "@/lib/external-navigation";
import { PRODUCT_NAME } from "@/product";

export const Route = createFileRoute("/cli-auth")({
  component: CliAuthPage,
});

function CliAuthPage() {
  const router = useRouter();
  // The raw query string: the router JSON-parses and re-serialises search values
  // (README § Conventions, Search params), which could change the CLI's values.
  const request = parseCliRequest(cliAuthSearchFrom(router.history.location.search));
  const { data: me } = $api.useSuspenseQuery("get", "/api/auth/me");
  // gcTime 0: the response holds the secret, which is never shown here (D-L-5).
  const create = $api.useMutation("post", "/api/v1/me/tokens", { gcTime: 0 });
  const [outcome, setOutcome] = useState<"pending" | "authorized" | "cancelled">("pending");

  if (request === undefined) {
    return (
      <AuthLayout
        title="This sign-in link is invalid"
        description="Run the login command in your terminal again to get a new link."
      >
        {null}
      </AuthLayout>
    );
  }

  if (outcome === "authorized") {
    return (
      <AuthLayout
        title="You're signed in"
        description="Return to your terminal. You can close this tab. If the terminal is still waiting, run the login command again and revoke the extra token on your account page."
      >
        {null}
      </AuthLayout>
    );
  }

  if (outcome === "cancelled") {
    return (
      <AuthLayout
        title="Sign-in cancelled"
        description="Nothing was created. You can close this tab."
      >
        {null}
      </AuthLayout>
    );
  }

  const authorize = () => {
    create.mutate(
      {
        body: {
          name: cliTokenName(request),
          scopes: ["read", "write"],
          expires_at: cliTokenExpiry(new Date()),
        },
      },
      {
        onSuccess: (data) => {
          const callback = cliCallbackForm(request, data.secret);
          create.reset();
          setOutcome("authorized");
          submitExternalForm(callback.action, callback.fields);
        },
      },
    );
  };

  const cancel = () => {
    setOutcome("cancelled");
    navigateExternal(cliCancelUrl(request));
  };

  return (
    <AuthLayout
      title={`Authorize the ${PRODUCT_NAME} CLI`}
      description={
        <>
          Signed in as <strong>{me.user.email}</strong>.
        </>
      }
    >
      <p role="note" className="rounded-md border p-3 text-sm font-medium">
        Only continue if you just ran <code>{`${PRODUCT_NAME} login`}</code> on this computer.
        Someone who sent you this link could otherwise get access to your account.
      </p>
      <p className="text-sm">
        Computer name reported by the CLI: <code>{request.host ?? "not reported"}</code>
      </p>
      <p className="text-sm">
        The CLI will get a personal access token named <strong>{cliTokenName(request)}</strong> with
        read and write access. It expires in 90 days. You can revoke it on your account page.
      </p>
      <FormError
        message={
          create.error
            ? problemMessage(create.error, "Could not create the token. Try again.")
            : undefined
        }
      />
      <div className="flex gap-2">
        <Button type="button" disabled={create.isPending} onClick={authorize}>
          {create.isPending ? <Loader2 className="animate-spin" aria-hidden="true" /> : null}
          Authorize
        </Button>
        <Button type="button" variant="outline" disabled={create.isPending} onClick={cancel}>
          Cancel
        </Button>
      </div>
    </AuthLayout>
  );
}
