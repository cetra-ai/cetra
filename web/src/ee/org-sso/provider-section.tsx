// SPDX-License-Identifier: Elastic-2.0
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage } from "@/api/problem";
import type { Schema } from "@/api/types";
import { DangerZone } from "@/components/deletions/danger-zone";
import { TextField } from "@/components/form/text-field";
import { LockedFeature } from "@/components/plan/locked-feature";
import { ReadOnlyCodeField } from "@/components/settings/read-only-code-field";
import { SettingsCard, SettingsFormCard } from "@/components/settings/settings-card";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { storeOrgSso } from "@/ee/org-sso/cache";
import { SamlProviderForm } from "@/ee/org-sso/saml-provider-form";
import { SamlSpCard } from "@/ee/org-sso/saml-sp-card";

const JIT_ROLES: Schema<"JitRole">[] = ["viewer", "member"];
const JIT_ROLE_LABELS: Record<Schema<"JitRole">, string> = {
  viewer: "Viewer",
  member: "Member",
};
const LOCKED_REASON = "Needs the Teams add-on or an enterprise licence";

type Field = "issuer" | "clientId" | "clientSecret";

/**
 * The org's identity provider: its protocol, its settings and the values to register at the
 * provider (D-P-52, D-SA-12). Removal is in `ProviderRemoval`. The parent remounts this section
 * when the provider is created, removed or switched to another protocol.
 */
export function ProviderSection({
  org,
  sso,
  locked,
  samlLocked,
  upgradeUrl,
}: {
  org: string;
  sso: Schema<"OrgSso">;
  /** `org_sso` is missing: the OpenID Connect form is locked. */
  locked: boolean;
  /** `org_sso` or `saml` is missing: the SAML form is locked. */
  samlLocked: boolean;
  upgradeUrl: string | undefined;
}) {
  const provider = sso.provider;
  const [protocol, setProtocol] = useState<Schema<"SsoProtocol">>(provider?.protocol ?? "oidc");
  const switching = provider !== undefined && protocol !== provider.protocol;
  return (
    <>
      <RadioGroup
        aria-label="Protocol"
        value={protocol}
        onValueChange={(value) => {
          setProtocol(value === "saml" ? "saml" : "oidc");
        }}
      >
        <div className="flex items-center gap-2">
          <RadioGroupItem value="oidc" id="sso-protocol-oidc" />
          <Label htmlFor="sso-protocol-oidc">OpenID Connect</Label>
        </div>
        <div className="flex items-center gap-2">
          <RadioGroupItem value="saml" id="sso-protocol-saml" />
          <Label htmlFor="sso-protocol-saml">SAML 2.0</Label>
        </div>
      </RadioGroup>
      {provider && switching ? (
        <p role="note" className="text-body text-muted-foreground">
          {provider.enforced
            ? "Turn off enforcement before switching the protocol."
            : "Saving replaces the current provider. Members link again at their next sign-in, and the provider must be tested and enabled again."}
        </p>
      ) : null}
      {protocol === "saml" ? (
        <>
          {samlLocked && !locked ? <LockedFeature feature="saml" upgradeUrl={upgradeUrl} /> : null}
          <SamlProviderForm org={org} sso={sso} locked={samlLocked} />
          <SamlSpCard sp={sso.saml_sp} />
        </>
      ) : (
        <OidcSection org={org} sso={sso} locked={locked} />
      )}
    </>
  );
}

/** The OpenID Connect form and the callback URL (D-P-52). */
function OidcSection({
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

  // Required fields: the issuer and client id always, the secret unless the saved provider is
  // OpenID Connect (a new provider, or one switched from SAML, needs a secret).
  const required: Partial<Record<Field, string>> = {
    ...(issuer.trim() === "" ? { issuer: "Enter the issuer URL." } : {}),
    ...(clientId.trim() === "" ? { clientId: "Enter a client ID." } : {}),
    ...(provider?.protocol !== "oidc" && clientSecret === ""
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
          placeholder={provider?.protocol === "oidc" ? "Stored; leave blank to keep it" : undefined}
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

/** Removes the org's identity provider, after its confirm text is typed (D-P-52). */
export function ProviderRemoval({ org, confirmText }: { org: string; confirmText: string }) {
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
        expected={confirmText}
        inputLabel={`Type "${confirmText}" to confirm`}
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
