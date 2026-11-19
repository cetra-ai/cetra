// SPDX-License-Identifier: Elastic-2.0
import { useQuery } from "@tanstack/react-query";
import { TriangleAlert } from "lucide-react";
import { adminLicenseQuery } from "@/api/queries-admin";
import type { Schema } from "@/api/types";
import { SettingsCard } from "@/components/settings/settings-card";
import { Skeleton } from "@/components/ui/skeleton";

const FEATURE_NAMES: Record<Schema<"Feature">, string> = {
  retention_management: "Data retention management",
  project_roles: "Per-project roles",
  sso_enforcement: "SSO enforcement",
  org_sso: "Single sign-on",
};

/** The licence on the admin settings page: status, customer, id and the features it lists. */
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
      <p className="text-body text-muted-foreground">
        Set the key with CETRA_LICENSE_KEY and restart the server.
      </p>
    </SettingsCard>
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
