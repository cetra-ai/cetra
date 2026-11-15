// SPDX-License-Identifier: Elastic-2.0
import { useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage } from "@/api/problem";
import type { Schema } from "@/api/types";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { CopyButton } from "@/components/copy-button";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { FormSuccess } from "@/components/form/form-success";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { storeOrgSso } from "@/ee/org-sso/cache";

const JIT_ROLES: Schema<"JitRole">[] = ["viewer", "member"];
const JIT_ROLE_LABELS: Record<Schema<"JitRole">, string> = {
  viewer: "Viewer",
  member: "Member",
};
const LOCKED_REASON = "Needs the Teams add-on or an enterprise licence";

/** The org's identity provider: its settings, the callback URL and removal (D-P-52). */
export function ProviderSection({
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
  const [displayName, setDisplayName] = useState(provider?.display_name ?? "");
  const [issuer, setIssuer] = useState(provider?.issuer ?? "");
  const [clientId, setClientId] = useState(provider?.client_id ?? "");
  const [clientSecret, setClientSecret] = useState("");
  const [jitRole, setJitRole] = useState<Schema<"JitRole">>(provider?.jit_role ?? "viewer");
  const [saved, setSaved] = useState(false);
  const [removeOpen, setRemoveOpen] = useState(false);
  const save = $api.useMutation("put", "/api/v1/orgs/{org}/sso/provider");
  const remove = $api.useMutation("delete", "/api/v1/orgs/{org}/sso/provider");

  const fields = fieldErrors(save.error);
  const saveError =
    save.error && Object.keys(fields).length === 0
      ? problemMessage(save.error, "Could not save the identity provider.")
      : undefined;

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    setSaved(false);
    save.reset();
    save.mutate(
      {
        params: { path: { org } },
        body: {
          display_name: displayName.trim(),
          issuer: issuer.trim(),
          client_id: clientId.trim(),
          jit_role: jitRole,
          // Blank keeps the stored secret (the server requires one only when creating).
          ...(clientSecret.length > 0 ? { client_secret: clientSecret } : {}),
        },
      },
      {
        onSuccess: (answer) => {
          setClientSecret("");
          setSaved(true);
          storeOrgSso(queryClient, org, answer);
        },
      },
    );
  }

  function confirmRemove() {
    remove.mutate(
      { params: { path: { org } } },
      {
        onSuccess: () => {
          setRemoveOpen(false);
          setDisplayName("");
          setIssuer("");
          setClientId("");
          setClientSecret("");
          setJitRole("viewer");
          storeOrgSso(queryClient, org, undefined);
        },
      },
    );
  }

  const removeError = remove.error
    ? problemMessage(remove.error, "Could not remove the identity provider.")
    : undefined;
  const formDisabled = locked || save.isPending;

  return (
    <section aria-labelledby="sso-provider-heading" className="flex flex-col gap-4">
      <div className="flex flex-col gap-1">
        <h2 id="sso-provider-heading" className="text-base font-medium">
          Identity provider
        </h2>
        <p className="text-sm text-muted-foreground">
          One OpenID Connect provider for this organization.
        </p>
      </div>
      <form
        noValidate
        aria-label="Identity provider"
        className="flex max-w-md flex-col gap-4"
        onSubmit={onSubmit}
      >
        <TextField
          id="sso-display-name"
          label="Display name"
          value={displayName}
          maxLength={100}
          disabled={formDisabled}
          error={fields.display_name}
          onChange={(value) => {
            setDisplayName(value);
            setSaved(false);
          }}
        />
        <TextField
          id="sso-issuer"
          label="Issuer URL"
          value={issuer}
          disabled={formDisabled}
          error={fields.issuer}
          onChange={(value) => {
            setIssuer(value);
            setSaved(false);
          }}
        />
        <TextField
          id="sso-client-id"
          label="Client ID"
          value={clientId}
          maxLength={255}
          disabled={formDisabled}
          error={fields.client_id}
          onChange={(value) => {
            setClientId(value);
            setSaved(false);
          }}
        />
        <TextField
          id="sso-client-secret"
          label="Client secret"
          type="password"
          autoComplete="new-password"
          value={clientSecret}
          disabled={formDisabled}
          error={fields.client_secret}
          placeholder={provider ? "Stored; leave blank to keep it" : undefined}
          onChange={(value) => {
            setClientSecret(value);
            setSaved(false);
          }}
        />
        <div className="flex flex-col gap-1.5">
          <Label htmlFor="sso-jit-role">Role for new members</Label>
          <Select
            value={jitRole}
            onValueChange={(value) => {
              const next = JIT_ROLES.find((role) => role === value);
              if (next) {
                setJitRole(next);
                setSaved(false);
              }
            }}
            disabled={formDisabled}
          >
            <SelectTrigger id="sso-jit-role" aria-label="Role for new members" className="w-fit">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {JIT_ROLES.map((role) => (
                <SelectItem key={role} value={role}>
                  {JIT_ROLE_LABELS[role]}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <p className="text-xs text-muted-foreground">Viewers do not use a seat.</p>
          <p className="text-xs text-muted-foreground">
            Anyone with a verified-domain email who signs in through the provider joins with this
            role, including people removed earlier; block someone at your identity provider.
          </p>
        </div>
        {provider ? null : (
          <p className="text-xs text-muted-foreground">
            Save the provider, then register its callback URL below.
          </p>
        )}
        <FormError message={saveError} />
        <FormSuccess message={saved ? "Saved" : undefined} />
        <DisabledReason reason={locked ? LOCKED_REASON : undefined}>
          <SubmitButton pending={save.isPending} disabled={locked}>
            Save
          </SubmitButton>
        </DisabledReason>
      </form>
      <div className="flex flex-col gap-2">
        <p className="text-sm font-medium">Callback URL</p>
        <div className="flex items-center gap-2">
          <code className="break-all rounded bg-muted px-2 py-1 text-xs">{sso.callback_url}</code>
          <CopyButton value={sso.callback_url} label="callback URL" />
        </div>
        <p className="text-xs text-muted-foreground">
          Register this redirect URI at your identity provider.
        </p>
      </div>
      {provider ? (
        <div>
          <Button
            type="button"
            variant="outline"
            onClick={() => {
              remove.reset();
              setRemoveOpen(true);
            }}
          >
            Remove provider
          </Button>
          <ConfirmDialog
            open={removeOpen}
            onOpenChange={setRemoveOpen}
            title="Remove the identity provider?"
            description="Sign-in through this provider stops and the identities linked to it are deleted. Turn enforcement off first."
            confirmLabel="Remove provider"
            destructive
            pending={remove.isPending}
            error={removeError}
            onConfirm={confirmRemove}
          />
        </div>
      ) : null}
    </section>
  );
}
