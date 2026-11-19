import { createFileRoute } from "@tanstack/react-router";
import { OrgSettingsHome } from "@/components/settings/settings-home";

export const Route = createFileRoute("/$org/settings/")({
  component: OrgSettingsIndexPage,
});

function OrgSettingsIndexPage() {
  const { org } = Route.useParams();
  return <OrgSettingsHome org={org} />;
}
