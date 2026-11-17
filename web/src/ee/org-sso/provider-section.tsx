// SPDX-License-Identifier: Elastic-2.0
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage } from "@/api/problem";
import type { Schema } from "@/api/types";
import { DangerZone } from "@/components/deletions/danger-zone";
import { TextField } from "@/components/form/text-field";
import { ReadOnlyCodeField } from "@/components/settings/read-only-code-field";
import { SettingsCard, SettingsFormCard } from "@/components/settings/settings-card";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
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

type Field = "issuer" | "clientId" | "clientSecret";

/**
 * The org's identity provider: its settings and the callback URL (D-P-52). Removal is in
 * `ProviderRemoval`. The parent remounts this form when the provider is created or removed.
 */
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
  const [submitted, setSubmitted] = useState(false);
  const [touched, setTouched] = useState<Partial<Record<Field, true>>>({});
  const save = $api.useMutation("put", "/api/v1/orgs/{org}/sso/provider");

  // Required fields: the issuer and client id always, the secret only when creating.
  const required: Partial<Record<Field, string>> = {
    ...(issuer.trim() === "" ? { issuer: "Enter the issuer URL." } : {}),
    ...(clientId.trim() === "" ? { clientId: "Enter a client ID." } : {}),
    ...(provider === undefined && clientSecret === ""
      ? { clientSecret: "Enter a client secret." }
      : {}),
  };
  const invalid = Object.keys(required).length > 0;
  const shownRequired = (field: Field) =>
    submitted || touched[field] === true ? required[field] : undefined;
  const dirty =
    clientSecret.length > 0 ||
    displayName.trim() !== (provider?.display_name ?? "") ||
    issuer.trim() !== (provider?.issuer ?? "") ||
    clientId.trim() !== (provider?.client_id ?? "") ||
    jitRole !== (provider?.jit_role ?? "viewer");

  const fields = fieldErrors(save.error);
  const saveError =
    save.error && Object.keys(fields).length === 0
      ? problemMessage(save.error, "Could not save the identity provider.")
      : undefined;

  function onSubmit() {
    setSaved(false);
    setSubmitted(true);
    save.reset();
    if (invalid) {
      return;
    }
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

  return (
    <>
      <SettingsFormCard
        title="Identity provider"
        description="One OpenID Connect provider for this organization."
        onSubmit={onSubmit}
        dirty={dirty}
        invalid={invalid}
        pending={save.isPending}
        saved={saved}
        error={saveError}
        disabledReason={locked ? LOCKED_REASON : undefined}
      >
        <TextField
          id="sso-display-name"
          label="Display name"
          value={displayName}
          maxLength={100}
          disabled={locked || save.isPending}
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
          disabled={locked || save.isPending}
          error={fields.issuer ?? shownRequired("issuer")}
          onChange={(value) => {
            setIssuer(value);
            setTouched((current) => ({ ...current, issuer: true }));
            setSaved(false);
          }}
        />
        <TextField
          id="sso-client-id"
          label="Client ID"
          value={clientId}
          maxLength={255}
          disabled={locked || save.isPending}
          error={fields.client_id ?? shownRequired("clientId")}
          onChange={(value) => {
            setClientId(value);
            setTouched((current) => ({ ...current, clientId: true }));
            setSaved(false);
          }}
        />
        <TextField
          id="sso-client-secret"
          label="Client secret"
          type="password"
          autoComplete="new-password"
          value={clientSecret}
          disabled={locked || save.isPending}
          error={fields.client_secret ?? shownRequired("clientSecret")}
          placeholder={provider ? "Stored; leave blank to keep it" : undefined}
          onChange={(value) => {
            setClientSecret(value);
            setTouched((current) => ({ ...current, clientSecret: true }));
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
            disabled={locked || save.isPending}
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
          <p className="text-caption text-muted-foreground">Viewers do not use a seat.</p>
          <p className="text-caption text-muted-foreground">
            Anyone with a verified-domain email who signs in through the provider joins with this
            role, including people removed earlier; block someone at your identity provider.
          </p>
        </div>
        {provider ? null : (
          <p className="text-caption text-muted-foreground">
            Save the provider, then register its callback URL below.
          </p>
        )}
      </SettingsFormCard>
      <SettingsCard
        title="Callback URL"
        description="Register this redirect URI at your identity provider."
      >
        <ReadOnlyCodeField id="sso-callback-url" label="Callback URL" value={sso.callback_url} />
      </SettingsCard>
    </>
  );
}

/** Removes the org's identity provider, after the issuer is typed (D-P-52). */
export function ProviderRemoval({ org, issuer }: { org: string; issuer: string }) {
  const queryClient = useQueryClient();
  const [open, setOpen] = useState(false);
  const remove = $api.useMutation("delete", "/api/v1/orgs/{org}/sso/provider");
  const removeError = remove.error
    ? problemMessage(remove.error, "Could not remove the identity provider.")
    : undefined;

  return (
    <DangerZone
      title="Remove the identity provider"
      description="Sign-in through this provider stops and the identities linked to it are deleted."
      buttonLabel="Remove provider…"
      onOpen={() => {
        remove.reset();
        setOpen(true);
      }}
    >
      <TypeToConfirmDialog
        open={open}
        onOpenChange={setOpen}
        title="Remove the identity provider?"
        description="Sign-in through this provider stops and the identities linked to it are deleted. Turn enforcement off first."
        expected={issuer}
        inputLabel={`Type "${issuer}" to confirm`}
        confirmLabel="Remove provider"
        pending={remove.isPending}
        error={removeError}
        onConfirm={() => {
          remove.mutate(
            { params: { path: { org } } },
            {
              onSuccess: () => {
                setOpen(false);
                storeOrgSso(queryClient, org, undefined);
              },
            },
          );
        }}
      />
    </DangerZone>
  );
}
