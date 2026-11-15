import { createFileRoute } from "@tanstack/react-router";
import { $api } from "@/api/client";
import { SettingsPage } from "@/components/settings/settings-page";
import { OrgSsoSettings } from "@/ee/org-sso/org-sso-settings";

export const Route = createFileRoute("/$org/settings/sso")({
  component: OrgSsoPage,
});

function OrgSsoPage() {
  const { org } = Route.useParams();
  const me = $api.useQuery("get", "/api/auth/me");
  const role = me.data?.orgs.find((candidate) => candidate.slug === org)?.role;
  return (
    <SettingsPage
      title="Single sign-on"
      description="Let people sign in with your organization's identity provider."
    >
      {role === "owner" ? <OrgSsoSettings org={org} /> : null}
      {role !== undefined && role !== "owner" ? (
        <p role="note" className="text-sm text-muted-foreground">
          Only owners can manage single sign-on.
        </p>
      ) : null}
    </SettingsPage>
  );
}
