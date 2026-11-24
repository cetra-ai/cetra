import { createFileRoute } from "@tanstack/react-router";
import { $api } from "@/api/client";
import { LockedFeature } from "@/components/plan/locked-feature";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { SettingsPage } from "@/components/settings/settings-page";
import { Skeleton } from "@/components/ui/skeleton";
import { AuditLogView } from "@/ee/audit/audit-log-view";
import { atLeast } from "@/lib/roles";
import { parseAuditSearch } from "@/lib/search/audit";

export const Route = createFileRoute("/$org/settings/audit-log")({
  validateSearch: (search) => parseAuditSearch(search),
  component: OrgAuditLogPage,
});

function OrgAuditLogPage() {
  const { org } = Route.useParams();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const me = $api.useQuery("get", "/api/auth/me");
  const { query, locked, upgradeUrl } = useOrgEntitlements(org);
  const role = me.data?.orgs.find((candidate) => candidate.slug === org)?.role;

  if (query.error) {
    throw query.error;
  }

  return (
    <SettingsPage
      title="Audit log"
      description="Records of sign-ins, membership, keys, settings and data exports in this organization."
      width="wide"
    >
      {role !== undefined && !atLeast(role, "admin") ? (
        <p role="note" className="text-body text-muted-foreground">
          Only admins and owners can read the audit log.
        </p>
      ) : null}
      {atLeast(role, "admin") && query.data === undefined ? (
        <Skeleton className="h-64 w-full" />
      ) : null}
      {atLeast(role, "admin") && locked("audit_log") ? (
        <LockedFeature feature="audit_log" upgradeUrl={upgradeUrl} />
      ) : null}
      {atLeast(role, "admin") && query.data !== undefined && !locked("audit_log") ? (
        <AuditLogView
          scope={{ org }}
          search={search}
          onUpdate={(patch) => {
            void navigate({ search: (previous) => ({ ...previous, ...patch }), replace: true });
          }}
        />
      ) : null}
    </SettingsPage>
  );
}
