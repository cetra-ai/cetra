// SPDX-License-Identifier: Elastic-2.0
import { useQuery } from "@tanstack/react-query";
import { problemMessage } from "@/api/problem";
import { orgSsoQuery } from "@/api/queries-settings";
import { FormError } from "@/components/form/form-error";
import { LockedFeature } from "@/components/plan/locked-feature";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { Skeleton } from "@/components/ui/skeleton";
import { DomainsSection } from "@/ee/org-sso/domains-section";
import { ProviderRemoval, ProviderSection } from "@/ee/org-sso/provider-section";
import { SamlMetadataCard } from "@/ee/org-sso/saml-metadata-card";
import { StatusSection } from "@/ee/org-sso/status-section";

/**
 * The org's single sign-on settings (owners only; the route checks the role). Without
 * `org_sso` the sections are read-only where a change would widen access (D-P-61). A SAML
 * provider also needs `saml` to be turned on or saved (D-P-61, D-SA-4).
 */
export function OrgSsoSettings({ org }: { org: string }) {
  const query = useQuery(orgSsoQuery(org));
  const entitlements = useOrgEntitlements(org);
  const locked = entitlements.locked("org_sso");
  const samlLocked = locked || entitlements.locked("saml");

  if (query.isPending) {
    return <Skeleton className="h-64 w-full" />;
  }
  if (query.data === undefined) {
    return <FormError message={problemMessage(query.error, "Could not load single sign-on.")} />;
  }

  const sso = query.data;
  const statusLocked = locked || (sso.provider?.protocol === "saml" && entitlements.locked("saml"));
  return (
    <div className="flex flex-col gap-6">
      {locked ? <LockedFeature feature="org_sso" upgradeUrl={entitlements.upgradeUrl} /> : null}
      {/* Keyed on the provider's creation and protocol, so a new, removed or switched provider starts a fresh form. */}
      <ProviderSection
        key={`${sso.provider?.created_at ?? "no-provider"}-${sso.provider?.protocol ?? ""}`}
        org={org}
        sso={sso}
        locked={locked}
        samlLocked={samlLocked}
        upgradeUrl={entitlements.upgradeUrl}
      />
      {sso.provider?.protocol === "saml" && sso.provider.saml ? (
        <SamlMetadataCard saml={sso.provider.saml} />
      ) : null}
      <DomainsSection org={org} sso={sso} locked={locked} />
      <StatusSection org={org} sso={sso} locked={statusLocked} />
      {sso.provider ? (
        <ProviderRemoval
          org={org}
          confirmText={
            sso.provider.issuer ?? sso.provider.saml?.idp_entity_id ?? sso.provider.display_name
          }
        />
      ) : null}
    </div>
  );
}
