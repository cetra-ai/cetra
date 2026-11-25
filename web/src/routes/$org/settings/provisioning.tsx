import { createFileRoute } from "@tanstack/react-router";
import { $api } from "@/api/client";
import { SettingsPage } from "@/components/settings/settings-page";
import { ProvisioningSettings } from "@/ee/scim/provisioning-settings";

export const Route = createFileRoute("/$org/settings/provisioning")({
  component: ProvisioningPage,
});

function ProvisioningPage() {
  const { org } = Route.useParams();
  const me = $api.useQuery("get", "/api/auth/me");
  const role = me.data?.orgs.find((candidate) => candidate.slug === org)?.role;
  return (
    <SettingsPage
      title="Provisioning"
      description="Create, update and remove members from your identity provider over SCIM 2.0."
    >
      {role === "owner" ? <ProvisioningSettings org={org} /> : null}
      {role !== undefined && role !== "owner" ? (
        <p role="note" className="text-body text-muted-foreground">
          Only owners can manage provisioning.
        </p>
      ) : null}
    </SettingsPage>
  );
}
