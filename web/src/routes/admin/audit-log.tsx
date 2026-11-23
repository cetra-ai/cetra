import { createFileRoute } from "@tanstack/react-router";
import { AuditLogView } from "@/ee/audit/audit-log-view";
import { parseAuditSearch } from "@/lib/search/audit";

export const Route = createFileRoute("/admin/audit-log")({
  validateSearch: (search) => parseAuditSearch(search),
  component: AdminAuditLogPage,
});

/** The instance's audit log: the records with no org (D-EA-8). */
function AdminAuditLogPage() {
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  return (
    <AuditLogView
      scope="instance"
      search={search}
      onUpdate={(patch) => {
        void navigate({ search: (previous) => ({ ...previous, ...patch }), replace: true });
      }}
    />
  );
}
