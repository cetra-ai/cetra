// SPDX-License-Identifier: Elastic-2.0
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { TriangleAlert } from "lucide-react";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { adminLicenseQuery, adminSettingsQuery } from "@/api/queries-admin";
import type { Schema } from "@/api/types";
import { SettingsCard, SettingsFormCard } from "@/components/settings/settings-card";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Textarea } from "@/components/ui/textarea";

const FEATURE_NAMES: Record<Schema<"Feature">, string> = {
  retention_management: "Data retention management",
  project_roles: "Per-project roles",
  sso_enforcement: "SSO enforcement",
  org_sso: "Single sign-on",
  saml: "SAML single sign-on",
};

/** The licence on the admin settings page: status, customer, id, the features it lists, and an install form. */
export function LicenseCard() {
  const license = useQuery(adminLicenseQuery);
  if (license.isPending) {
    return <Skeleton className="h-24 w-full" />;
  }
  if (license.error) {
    throw license.error;
  }
  const data = license.data;

  return (
    <>
      <SettingsCard title="Licence">
        <dl className="grid grid-cols-[minmax(10rem,max-content)_1fr] gap-x-6 gap-y-2 text-body">
          <dt className="text-muted-foreground">Status</dt>
          <dd>
            {data.status === "grace" ? (
              <TriangleAlert
                className="mr-1 inline size-4 align-text-bottom text-destructive"
                aria-hidden="true"
              />
            ) : null}
            {statusLabel(data)}
          </dd>
          {data.customer ? (
            <>
              <dt className="text-muted-foreground">Customer</dt>
              <dd>{data.customer}</dd>
            </>
          ) : null}
          {data.id ? (
            <>
              <dt className="text-muted-foreground">Licence id</dt>
              <dd className="font-mono">{data.id}</dd>
            </>
          ) : null}
          <dt className="text-muted-foreground">Features</dt>
          <dd>
            {data.features.length > 0
              ? data.features.map((feature) => FEATURE_NAMES[feature]).join(", ")
              : "None"}
          </dd>
        </dl>
        {data.source === "config" ? (
          <p className="text-body text-muted-foreground">
            Set by CETRA_LICENSE_KEY or license_key in the configuration. Change it there and
            restart the server.
          </p>
        ) : data.source === "stored" ? (
          <p className="text-body text-muted-foreground">Installed from this page.</p>
        ) : null}
        {data.renew_url ? (
          <p className="text-body">
            <a href={data.renew_url} target="_blank" rel="noreferrer" className="underline">
              Buy or renew a licence
            </a>
          </p>
        ) : null}
      </SettingsCard>
      {data.source !== "config" ? <InstallLicenseForm /> : null}
    </>
  );
}

/** Installs a pasted licence key; it applies at once, without a restart (D-P-75). */
function InstallLicenseForm() {
  const queryClient = useQueryClient();
  const [key, setKey] = useState("");
  const [saved, setSaved] = useState(false);
  const install = $api.useMutation("put", "/api/v1/admin/license");

  function onSubmit() {
    setSaved(false);
    install.reset();
    if (key.trim() === "") {
      return;
    }
    install.mutate(
      { body: { key: key.trim() } },
      {
        onSuccess: (answer) => {
          setKey("");
          setSaved(true);
          queryClient.setQueryData(adminLicenseQuery.queryKey, answer);
          // enforce_sso_applied depends on the licence; every org's plan does too.
          void queryClient.invalidateQueries({ queryKey: adminSettingsQuery.queryKey });
          void queryClient.invalidateQueries({
            predicate: (query) => query.queryKey[1] === "/api/v1/orgs/{org}/entitlements",
          });
        },
      },
    );
  }

  return (
    <SettingsFormCard
      title="Install a licence key"
      description="Paste the key. It applies at once, without a restart."
      onSubmit={onSubmit}
      dirty={key !== ""}
      invalid={key.trim() === ""}
      pending={install.isPending}
      saved={saved}
      error={
        install.error
          ? problemMessage(install.error, "Could not install the licence key.")
          : undefined
      }
      saveLabel="Install"
    >
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="license-key">Licence key</Label>
        <Textarea
          id="license-key"
          value={key}
          onChange={(event) => {
            setKey(event.target.value);
            setSaved(false);
          }}
          rows={4}
          className="font-mono"
          spellCheck={false}
          autoComplete="off"
        />
      </div>
    </SettingsFormCard>
  );
}

function statusLabel(license: Schema<"AdminLicense">): string {
  switch (license.status) {
    case "none":
      return "No licence key. Enterprise features are locked.";
    case "invalid":
      return `The licence key was refused: ${license.reason ?? "no reason given"}.`;
    case "active":
      return `Active until ${day(license.expires_at)}`;
    case "grace":
      return `Expired on ${day(license.expires_at)}. Enterprise features stop on ${day(license.grace_ends_at)}.`;
    case "expired":
      return `Expired on ${day(license.expires_at)}. Enterprise features are locked.`;
  }
}

/** The UTC day of an RFC 3339 instant; licence instants are whole UTC days. */
function day(iso: string | undefined): string {
  return iso?.slice(0, 10) ?? "";
}
