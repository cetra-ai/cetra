// SPDX-License-Identifier: Elastic-2.0
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import type { Schema } from "@/api/types";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { FormError } from "@/components/form/form-error";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { storeOrgSso } from "@/ee/org-sso/cache";
import { ssoAuthorizeHref } from "@/lib/redirect";

/**
 * Enabled (discovery sends people to the provider, D-P-57) and Enforced (password and
 * token sign-in stop for the org's domains, D-P-58). Without `org_sso` a switch can only
 * be turned off (D-P-61).
 */
export function StatusSection({
  org,
  sso,
  locked,
}: {
  org: string;
  sso: Schema<"OrgSso">;
  locked: boolean;
}) {
  const provider = sso.provider;
  const queryClient = useQueryClient();
  const [enforceOpen, setEnforceOpen] = useState(false);
  const patch = $api.useMutation("patch", "/api/v1/orgs/{org}/sso");

  const verifiedDomains = sso.domains.filter((row) => row.status === "verified");
  const firstVerified = verifiedDomains[0]?.domain;
  const patchError = patch.error
    ? problemMessage(patch.error, "Could not change single sign-on.")
    : undefined;

  function update(flags: Schema<"PatchOrgSso">) {
    patch.reset();
    patch.mutate(
      { params: { path: { org } }, body: flags },
      {
        onSuccess: (answer) => {
          setEnforceOpen(false);
          storeOrgSso(queryClient, org, answer);
        },
      },
    );
  }

  const switchesDisabled = provider === undefined || patch.isPending;

  return (
    <section aria-labelledby="sso-status-heading" className="flex flex-col gap-4">
      <h2 id="sso-status-heading" className="text-base font-medium">
        Status
      </h2>
      <div className="flex flex-col gap-3">
        <div className="flex items-center gap-2">
          <Switch
            id="sso-enabled"
            checked={provider?.enabled ?? false}
            disabled={switchesDisabled || (locked && !provider?.enabled)}
            onCheckedChange={(enabled) => {
              update({ enabled });
            }}
          />
          <Label htmlFor="sso-enabled">Enabled</Label>
        </div>
        <p className="text-xs text-muted-foreground">
          Send people with these domains to the identity provider when they sign in.
        </p>
        <div className="flex items-center gap-2">
          <Switch
            id="sso-enforced"
            checked={provider?.enforced ?? false}
            disabled={switchesDisabled || (locked && !provider?.enforced)}
            onCheckedChange={(enforced) => {
              if (enforced) {
                patch.reset();
                setEnforceOpen(true);
                return;
              }
              update({ enforced: false });
            }}
          />
          <Label htmlFor="sso-enforced">Enforced</Label>
        </div>
        <p className="text-xs text-muted-foreground">
          Require it: password sign-in and personal access tokens stop working for these domains.
          API keys keep working.
        </p>
        {provider ? null : (
          <p className="text-xs text-muted-foreground">Save an identity provider first.</p>
        )}
        <FormError message={enforceOpen ? undefined : patchError} />
      </div>
      {provider && firstVerified !== undefined ? (
        <div className="flex flex-col gap-1">
          <a
            href={ssoAuthorizeHref(firstVerified, `/${org}/settings/sso`)}
            className="w-fit text-sm underline underline-offset-4"
          >
            Test sign-in
          </a>
          <p className="text-xs text-muted-foreground">
            Sign in through the provider once before you enforce it.
          </p>
        </div>
      ) : null}
      <ConfirmDialog
        open={enforceOpen}
        onOpenChange={setEnforceOpen}
        title="Enforce single sign-on?"
        description={
          <>
            {verifiedDomains.length > 0 ? (
              <>
                People with these verified domains lose password sign-in and personal access tokens
                at once, and must sign in through the provider:{" "}
                {verifiedDomains.map((row) => row.domain).join(", ")}.
              </>
            ) : (
              "This organization has no verified domains, so enforcement cannot be turned on yet."
            )}{" "}
            API keys keep working. Instance admins are not affected.
          </>
        }
        confirmLabel="Enforce"
        destructive
        pending={patch.isPending}
        error={enforceOpen ? patchError : undefined}
        onConfirm={() => {
          update({ enforced: true });
        }}
      />
    </section>
  );
}
