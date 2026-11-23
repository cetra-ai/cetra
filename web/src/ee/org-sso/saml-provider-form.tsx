// SPDX-License-Identifier: Elastic-2.0
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import type { Schema } from "@/api/types";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { TextField } from "@/components/form/text-field";
import { SettingsFormCard } from "@/components/settings/settings-card";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { storeOrgSso } from "@/ee/org-sso/cache";
import {
  attributesBody,
  ENTITY_ID_CHANGE_DETAIL,
  splitCertificates,
} from "@/ee/org-sso/saml";

const JIT_ROLES: Schema<"JitRole">[] = ["viewer", "member"];
const JIT_ROLE_LABELS: Record<Schema<"JitRole">, string> = {
  viewer: "Viewer",
  member: "Member",
};
const LOCKED_REASON = "Needs an enterprise licence that includes SAML";

/** Where the metadata comes from: the stored one, a URL, pasted XML, or manual fields. */
type Source = "current" | "url" | "xml" | "manual";
type RequiredField =
  | "displayName"
  | "metadataUrl"
  | "metadataXml"
  | "entityId"
  | "ssoUrl"
  | "certificates";

/** The API field names this form shows an error under. Any other field error shows at the top. */
const RENDERED_FIELDS = [
  "display_name",
  "metadata",
  "metadata_url",
  "metadata_xml",
  "idp_entity_id",
  "sso_url",
  "certificates",
  "attributes.email",
  "attributes.display_name",
  "attributes.first_name",
  "attributes.last_name",
];

function parseSource(value: string): Source {
  return value === "current" || value === "url" || value === "xml" || value === "manual"
    ? value
    : "url";
}

/** The 409 that asks the owner to confirm an entity ID change (M2 SA2.4). */
function isEntityIdChange(error: unknown): boolean {
  const problem = problemOf(error);
  return problem?.status === 409 && problem.detail === ENTITY_ID_CHANGE_DETAIL;
}

/**
 * The SAML provider form on the Single sign-on page (D-SA-2, D-SA-6, D-SA-11, D-SA-18). The
 * parent passes the saved provider through `sso`.
 */
export function SamlProviderForm({
  org,
  sso,
  locked,
}: {
  org: string;
  sso: Schema<"OrgSso">;
  /** `org_sso` or `saml` is missing: saving is locked (D-SA-4). */
  locked: boolean;
}) {
  const provider = sso.provider;
  const saml = provider?.protocol === "saml" ? provider.saml : undefined;
  const queryClient = useQueryClient();

  const [displayName, setDisplayName] = useState(provider?.display_name ?? "");
  const [jitRole, setJitRole] = useState<Schema<"JitRole">>(provider?.jit_role ?? "viewer");
  const [source, setSource] = useState<Source>(saml ? "current" : "url");
  const [metadataUrl, setMetadataUrl] = useState(saml?.metadata_url ?? "");
  const [metadataXml, setMetadataXml] = useState("");
  const [entityId, setEntityId] = useState("");
  const [ssoUrl, setSsoUrl] = useState("");
  const [certificates, setCertificates] = useState("");
  const [attributeEmail, setAttributeEmail] = useState(saml?.attributes.email ?? "");
  const [attributeDisplayName, setAttributeDisplayName] = useState(
    saml?.attributes.display_name ?? "",
  );
  const [attributeFirstName, setAttributeFirstName] = useState(saml?.attributes.first_name ?? "");
  const [attributeLastName, setAttributeLastName] = useState(saml?.attributes.last_name ?? "");
  const [saved, setSaved] = useState(false);
  const [submitted, setSubmitted] = useState(false);
  const [touched, setTouched] = useState<Partial<Record<RequiredField, true>>>({});
  const [pendingBody, setPendingBody] = useState<Schema<"PutOrgSamlProvider">>();
  const [confirmOpen, setConfirmOpen] = useState(false);
  const save = $api.useMutation("put", "/api/v1/orgs/{org}/sso/saml");
  const confirm = $api.useMutation("put", "/api/v1/orgs/{org}/sso/saml");

  const disabled = locked || save.isPending;

  // Required fields for the chosen source; the Save button stays off while any is missing.
  const required: Partial<Record<RequiredField, string>> = {
    ...(displayName.trim() === "" ? { displayName: "Enter a display name." } : {}),
    ...(source === "url" && metadataUrl.trim() === ""
      ? { metadataUrl: "Enter the metadata URL." }
      : {}),
    ...(source === "xml" && metadataXml.trim() === ""
      ? { metadataXml: "Paste the metadata XML." }
      : {}),
    ...(source === "manual" && entityId.trim() === ""
      ? { entityId: "Enter the IdP entity ID." }
      : {}),
    ...(source === "manual" && ssoUrl.trim() === "" ? { ssoUrl: "Enter the sign-on URL." } : {}),
    ...(source === "manual" && splitCertificates(certificates).length === 0
      ? { certificates: "Paste at least one certificate." }
      : {}),
  };
  const invalid = Object.keys(required).length > 0;
  const shownRequired = (field: RequiredField) =>
    submitted || touched[field] === true ? required[field] : undefined;
  const touch = (field: RequiredField) => {
    setTouched((current) => ({ ...current, [field]: true }));
  };

  // A SAML provider is always saved by this form, so an unsaved form counts as dirty.
  const dirty =
    !locked &&
    (saml === undefined ||
      source !== "current" ||
      displayName.trim() !== (provider?.display_name ?? "") ||
      jitRole !== (provider?.jit_role ?? "viewer") ||
      attributeEmail.trim() !== (saml.attributes.email ?? "") ||
      attributeDisplayName.trim() !== (saml.attributes.display_name ?? "") ||
      attributeFirstName.trim() !== (saml.attributes.first_name ?? "") ||
      attributeLastName.trim() !== (saml.attributes.last_name ?? ""));

  const fields = fieldErrors(save.error);
  const unrenderedField = Object.keys(fields).find((key) => !RENDERED_FIELDS.includes(key));
  const saveError = (() => {
    if (!save.error || confirmOpen) {
      return undefined;
    }
    if (unrenderedField !== undefined) {
      return fields[unrenderedField];
    }
    return Object.keys(fields).length === 0
      ? problemMessage(save.error, "Could not save the identity provider.")
      : undefined;
  })();

  function buildBody(): Schema<"PutOrgSamlProvider"> {
    const body: Schema<"PutOrgSamlProvider"> = {
      display_name: displayName.trim(),
      jit_role: jitRole,
    };
    if (source === "url") {
      body.metadata_url = metadataUrl.trim();
    } else if (source === "xml") {
      body.metadata_xml = metadataXml;
    } else if (source === "manual") {
      body.idp_entity_id = entityId.trim();
      body.sso_url = ssoUrl.trim();
      body.certificates = splitCertificates(certificates);
    }
    const attributes = attributesBody({
      email: attributeEmail,
      display_name: attributeDisplayName,
      first_name: attributeFirstName,
      last_name: attributeLastName,
    });
    if (Object.keys(attributes).length > 0) {
      body.attributes = attributes;
    }
    return body;
  }

  function saveSucceeded(answer: Schema<"OrgSso">) {
    setMetadataXml("");
    setEntityId("");
    setSsoUrl("");
    setCertificates("");
    setSource("current");
    setSaved(true);
    storeOrgSso(queryClient, org, answer);
  }

  function onSubmit() {
    setSaved(false);
    setSubmitted(true);
    save.reset();
    confirm.reset();
    if (invalid) {
      return;
    }
    const body = buildBody();
    setPendingBody(body);
    save.mutate(
      { params: { path: { org } }, body },
      {
        onSuccess: (answer) => {
          saveSucceeded(answer);
        },
        onError: (error) => {
          if (isEntityIdChange(error)) {
            setConfirmOpen(true);
          }
        },
      },
    );
  }

  function confirmEntityIdChange() {
    if (pendingBody === undefined) {
      return;
    }
    confirm.mutate(
      {
        params: { path: { org } },
        body: { ...pendingBody, confirm_entity_id_change: true },
      },
      {
        onSuccess: (answer) => {
          save.reset();
          setConfirmOpen(false);
          saveSucceeded(answer);
        },
      },
    );
  }

  const xmlError = fields.metadata_xml ?? shownRequired("metadataXml");

  return (
    <SettingsFormCard
      title="Identity provider"
      description="One SAML 2.0 identity provider for this organization."
      ariaLabel="Identity provider"
      onSubmit={onSubmit}
      dirty={dirty}
      invalid={invalid}
      pending={save.isPending}
      saved={saved}
      error={saveError}
      disabledReason={locked ? LOCKED_REASON : undefined}
    >
      <TextField
        id="saml-display-name"
        label="Display name"
        value={displayName}
        maxLength={100}
        disabled={disabled}
        error={fields.display_name ?? shownRequired("displayName")}
        onChange={(value) => {
          setDisplayName(value);
          touch("displayName");
          setSaved(false);
        }}
      />

      <Tabs
        value={source}
        onValueChange={(value) => {
          setSource(parseSource(value));
          setSaved(false);
        }}
      >
        <TabsList aria-label="Metadata source">
          {saml ? (
            <TabsTrigger value="current" disabled={disabled}>
              Current metadata
            </TabsTrigger>
          ) : null}
          <TabsTrigger value="url" disabled={disabled}>
            Metadata URL
          </TabsTrigger>
          <TabsTrigger value="xml" disabled={disabled}>
            Metadata XML
          </TabsTrigger>
          <TabsTrigger value="manual" disabled={disabled}>
            Enter manually
          </TabsTrigger>
        </TabsList>

        {saml ? (
          <TabsContent value="current">
            <p className="text-body text-muted-foreground">
              The stored metadata is kept. Choose another tab to replace it.
            </p>
          </TabsContent>
        ) : null}

        <TabsContent value="url" className="flex flex-col gap-4">
          <TextField
            id="saml-metadata-url-input"
            label="Metadata URL"
            placeholder="https://idp.example.com/app/…/sso/saml/metadata"
            description="cetra loads it now and checks it again once a day for new certificates."
            value={metadataUrl}
            disabled={disabled}
            error={fields.metadata_url ?? shownRequired("metadataUrl")}
            onChange={(value) => {
              setMetadataUrl(value);
              touch("metadataUrl");
              setSaved(false);
            }}
          />
        </TabsContent>

        <TabsContent value="xml" className="flex flex-col gap-1.5">
          <Label htmlFor="saml-metadata-xml">Metadata XML</Label>
          <Textarea
            id="saml-metadata-xml"
            value={metadataXml}
            disabled={disabled}
            aria-invalid={xmlError ? true : undefined}
            aria-describedby={xmlError ? "saml-metadata-xml-error" : undefined}
            onChange={(event) => {
              setMetadataXml(event.target.value);
              touch("metadataXml");
              setSaved(false);
            }}
          />
          <p className="text-caption text-muted-foreground">
            Paste the XML your identity provider shows for this app. It is not refreshed.
          </p>
          {xmlError ? (
            <p id="saml-metadata-xml-error" className="text-caption text-destructive">
              {xmlError}
            </p>
          ) : null}
        </TabsContent>

        <TabsContent value="manual" className="flex flex-col gap-4">
          <TextField
            id="saml-idp-entity-id-input"
            label="IdP entity ID"
            value={entityId}
            disabled={disabled}
            error={fields.idp_entity_id ?? shownRequired("entityId")}
            onChange={(value) => {
              setEntityId(value);
              touch("entityId");
              setSaved(false);
            }}
          />
          <TextField
            id="saml-sso-url-input"
            label="Sign-on URL"
            description="The identity provider's SSO URL for the HTTP-Redirect binding."
            value={ssoUrl}
            disabled={disabled}
            error={fields.sso_url ?? shownRequired("ssoUrl")}
            onChange={(value) => {
              setSsoUrl(value);
              touch("ssoUrl");
              setSaved(false);
            }}
          />
          <CertificatesField
            value={certificates}
            disabled={disabled}
            error={fields.certificates ?? shownRequired("certificates")}
            onChange={(value) => {
              setCertificates(value);
              touch("certificates");
              setSaved(false);
            }}
          />
        </TabsContent>
      </Tabs>
      {fields.metadata ? (
        <p role="alert" className="text-caption text-destructive">
          {fields.metadata}
        </p>
      ) : null}

      <div className="flex flex-col gap-1.5">
        <Label htmlFor="saml-jit-role">Role for new members</Label>
        <Select
          value={jitRole}
          onValueChange={(value) => {
            const next = JIT_ROLES.find((role) => role === value);
            if (next) {
              setJitRole(next);
              setSaved(false);
            }
          }}
          disabled={disabled}
        >
          <SelectTrigger id="saml-jit-role" aria-label="Role for new members" className="w-fit">
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

      <div className="flex flex-col gap-4">
        <h3 className="text-ui font-medium">Attribute names</h3>
        <p className="text-caption text-muted-foreground">
          Leave blank to use the usual names of Okta, Microsoft Entra ID, Google and Keycloak.
        </p>
        <TextField
          id="saml-attr-email"
          label="Email attribute"
          placeholder="email"
          value={attributeEmail}
          disabled={disabled}
          error={fields["attributes.email"]}
          onChange={(value) => {
            setAttributeEmail(value);
            setSaved(false);
          }}
        />
        <TextField
          id="saml-attr-display-name"
          label="Display name attribute"
          placeholder="displayName"
          value={attributeDisplayName}
          disabled={disabled}
          error={fields["attributes.display_name"]}
          onChange={(value) => {
            setAttributeDisplayName(value);
            setSaved(false);
          }}
        />
        <TextField
          id="saml-attr-first-name"
          label="First name attribute"
          placeholder="firstName"
          value={attributeFirstName}
          disabled={disabled}
          error={fields["attributes.first_name"]}
          onChange={(value) => {
            setAttributeFirstName(value);
            setSaved(false);
          }}
        />
        <TextField
          id="saml-attr-last-name"
          label="Last name attribute"
          placeholder="lastName"
          value={attributeLastName}
          disabled={disabled}
          error={fields["attributes.last_name"]}
          onChange={(value) => {
            setAttributeLastName(value);
            setSaved(false);
          }}
        />
      </div>

      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={(open) => {
          if (!open) {
            setConfirmOpen(false);
            save.reset();
            confirm.reset();
          }
        }}
        title="Change the identity provider?"
        description="The new metadata names another entity ID. Saving it deletes the identities linked to the current one; members link again at their next sign-in."
        confirmLabel="Save and relink"
        destructive
        pending={confirm.isPending}
        error={
          confirm.error
            ? problemMessage(confirm.error, "Could not save the identity provider.")
            : undefined
        }
        details={saml ? `Current IdP entity ID: ${saml.idp_entity_id}` : undefined}
        onConfirm={confirmEntityIdChange}
      />
    </SettingsFormCard>
  );
}

/** The signing certificates textarea, with its error under it. */
function CertificatesField({
  value,
  disabled,
  error,
  onChange,
}: {
  value: string;
  disabled: boolean;
  error: string | undefined;
  onChange: (value: string) => void;
}) {
  return (
    <div className="flex flex-col gap-1.5">
      <Label htmlFor="saml-certificates">Signing certificates</Label>
      <Textarea
        id="saml-certificates"
        value={value}
        disabled={disabled}
        aria-invalid={error ? true : undefined}
        aria-describedby={error ? "saml-certificates-error" : undefined}
        onChange={(event) => {
          onChange(event.target.value);
        }}
      />
      <p className="text-caption text-muted-foreground">PEM, one or more. Up to 4.</p>
      {error ? (
        <p id="saml-certificates-error" className="text-caption text-destructive">
          {error}
        </p>
      ) : null}
    </div>
  );
}
