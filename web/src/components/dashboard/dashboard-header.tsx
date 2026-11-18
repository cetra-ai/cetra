import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { ChevronDown, MoreHorizontal } from "lucide-react";
import { useState } from "react";
import { dashboardsQuery } from "@/api/dashboards";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { OVERVIEW } from "@/lib/dashboards/overview";
import type { Dashboard } from "@/lib/dashboards/types";
import { canManageDashboards } from "@/lib/roles";
import {
  DeleteDashboardDialog,
  DuplicateDashboardDialog,
  NewDashboardDialog,
  RenameDashboardDialog,
} from "./dashboard-dialogs";

/** A custom dashboard's name is at most 100 characters; a copy's prefix must fit too. */
const MAX_NAME_CHARS = 100;

/** Opens a dashboard by id, or the Overview, with the URL's range, filter and flags kept. */
function useOpenDashboard() {
  const { org, project } = useProject();
  const navigate = useNavigate();
  return (id: string | undefined) => {
    if (id === undefined) {
      void navigate({
        to: "/$org/$project/dashboard",
        params: { org, project },
        search: (prev) => prev,
      });
      return;
    }
    void navigate({
      to: "/$org/$project/dashboard/$dashboardId",
      params: { org, project, dashboardId: id },
      search: (prev) => prev,
    });
  };
}

/**
 * The dashboard switcher: the Overview, then the project's dashboards by name, then "New
 * dashboard" for members. Its button shows the current dashboard's name.
 */
export function DashboardSwitcher({ dashboard }: { dashboard: Dashboard | undefined }) {
  const { org, project, data } = useProject();
  const open = useOpenDashboard();
  const list = useQuery(dashboardsQuery(org, project));
  const [creating, setCreating] = useState(false);
  const canManage = canManageDashboards(data.role);
  const current = dashboard?.name ?? "Overview";

  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button type="button" variant="outline" size="sm" aria-label="Switch dashboard">
            <span className="max-w-64 truncate">{current}</span>
            <ChevronDown />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start" className="min-w-56">
          <DropdownMenuItem onSelect={() => open(undefined)}>Overview</DropdownMenuItem>
          {(list.data?.data ?? []).map((item) => (
            <DropdownMenuItem key={item.id} onSelect={() => open(item.id)}>
              {item.name}
            </DropdownMenuItem>
          ))}
          {canManage ? (
            <>
              <DropdownMenuSeparator />
              <DropdownMenuItem onSelect={() => setCreating(true)}>New dashboard</DropdownMenuItem>
            </>
          ) : null}
        </DropdownMenuContent>
      </DropdownMenu>
      {canManage ? (
        <NewDashboardDialog
          org={org}
          project={project}
          open={creating}
          onOpenChange={setCreating}
          onCreated={(created) => {
            setCreating(false);
            open(created.id);
          }}
        />
      ) : null}
    </>
  );
}

/**
 * The "⋯" menu of the open dashboard, for members. A custom dashboard can be renamed, duplicated
 * and deleted; the Overview only duplicated. Delete opens the Overview.
 */
export function DashboardMenu({ dashboard }: { dashboard: Dashboard | undefined }) {
  const { org, project, data } = useProject();
  const open = useOpenDashboard();
  const [dialog, setDialog] = useState<"rename" | "duplicate" | "delete" | null>(null);
  if (!canManageDashboards(data.role)) {
    return null;
  }
  const definition = dashboard?.definition ?? OVERVIEW;
  const defaultName = `Copy of ${dashboard?.name ?? "Overview"}`.slice(0, MAX_NAME_CHARS);

  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button type="button" variant="ghost" size="icon-sm" aria-label="Dashboard actions">
            <MoreHorizontal />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="min-w-40">
          {dashboard ? (
            <DropdownMenuItem onSelect={() => setDialog("rename")}>Rename</DropdownMenuItem>
          ) : null}
          <DropdownMenuItem onSelect={() => setDialog("duplicate")}>Duplicate</DropdownMenuItem>
          {dashboard ? (
            <DropdownMenuItem variant="destructive" onSelect={() => setDialog("delete")}>
              Delete
            </DropdownMenuItem>
          ) : null}
        </DropdownMenuContent>
      </DropdownMenu>
      {dashboard ? (
        <RenameDashboardDialog
          org={org}
          project={project}
          dashboard={dashboard}
          open={dialog === "rename"}
          onOpenChange={(next) => setDialog(next ? "rename" : null)}
          onSaved={() => setDialog(null)}
        />
      ) : null}
      <DuplicateDashboardDialog
        org={org}
        project={project}
        defaultName={defaultName}
        definition={definition}
        open={dialog === "duplicate"}
        onOpenChange={(next) => setDialog(next ? "duplicate" : null)}
        onCreated={(copy) => {
          setDialog(null);
          open(copy.id);
        }}
      />
      {dashboard ? (
        <DeleteDashboardDialog
          org={org}
          project={project}
          dashboard={dashboard}
          open={dialog === "delete"}
          onOpenChange={(next) => setDialog(next ? "delete" : null)}
          onDeleted={() => {
            setDialog(null);
            open(undefined);
          }}
        />
      ) : null}
    </>
  );
}
