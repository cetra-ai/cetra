import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { ChevronDown, MoreHorizontal } from "lucide-react";
import { useEffect, useState } from "react";
import { dashboardsQuery } from "@/api/dashboards";
import { projectRef } from "@/api/project-ref";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { peekDraft, subscribeDrafts, takeDraft } from "@/lib/assistant/handoff";
import type { DashboardDraft } from "@/lib/assistant/types";
import { OVERVIEW } from "@/lib/dashboards/overview";
import { useDashboardStore } from "@/lib/dashboards/store";
import type { Dashboard } from "@/lib/dashboards/types";
import { canManageDashboards } from "@/lib/roles";
import type { DashboardTarget } from "./dashboard-body";
import {
  DeleteDashboardDialog,
  DuplicateDashboardDialog,
  NewDashboardDialog,
  RenameDashboardDialog,
  ResetOverviewDialog,
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
  const { store, local } = useDashboardStore(org, project);
  const list = useQuery(dashboardsQuery(org, project, store));
  const [creating, setCreating] = useState(false);
  // The new dashboard's prefill: an assistant's draft handed over (D-AS-28).
  const [initial, setInitial] = useState<DashboardDraft | undefined>();
  // A demo-org viewer manages the dashboards kept in this browser (D-C-17).
  const canManage = local || canManageDashboards(data.role);
  const current = dashboard?.name ?? "Overview";
  const ref = projectRef(org, project);

  function setCreatingOpen(next: boolean) {
    setCreating(next);
    if (!next) {
      setInitial(undefined);
    }
  }

  // A new-dashboard draft opens the dialog prefilled, on mount and when handed over while open.
  useEffect(() => {
    function takeNewDraft() {
      const draft = peekDraft(ref, "dashboard");
      if (draft !== undefined && draft.dashboard_id === undefined && canManage) {
        takeDraft(ref, "dashboard");
        setInitial(draft);
        setCreating(true);
      }
    }
    takeNewDraft();
    return subscribeDrafts(ref, takeNewDraft);
  }, [ref, canManage]);

  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            aria-label="Switch dashboard"
            className="h-7 gap-1 px-1.5 font-medium"
          >
            <span className="max-w-64 truncate">{current}</span>
            <ChevronDown />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start" className="min-w-56">
          <DropdownMenuItem onSelect={() => open(undefined)}>Overview</DropdownMenuItem>
          {(list.data ?? []).map((item) => (
            <DropdownMenuItem key={item.id} onSelect={() => open(item.id)}>
              {item.name}
            </DropdownMenuItem>
          ))}
          {canManage ? (
            <>
              <DropdownMenuSeparator />
              <DropdownMenuItem onSelect={() => setCreatingOpen(true)}>
                New dashboard
              </DropdownMenuItem>
            </>
          ) : null}
        </DropdownMenuContent>
      </DropdownMenu>
      {canManage ? (
        <NewDashboardDialog
          org={org}
          project={project}
          open={creating}
          onOpenChange={setCreatingOpen}
          initial={initial}
          onCreated={(created) => {
            setCreatingOpen(false);
            open(created.id);
          }}
        />
      ) : null}
    </>
  );
}

/**
 * The "⋯" menu of the open dashboard, for members. A custom dashboard can be renamed, duplicated
 * and deleted. The Overview can be duplicated, and a stored one reset to the built-in preset; it
 * is never renamed or deleted. Delete opens the Overview.
 */
export function DashboardMenu({
  target,
  layoutBusy,
}: {
  target: DashboardTarget;
  /** A layout save is pending or running: a reset waits for it. */
  layoutBusy: boolean;
}) {
  const { org, project, data } = useProject();
  const open = useOpenDashboard();
  const { local } = useDashboardStore(org, project);
  const [dialog, setDialog] = useState<"rename" | "duplicate" | "delete" | "reset" | null>(null);
  if (!local && !canManageDashboards(data.role)) {
    return null;
  }
  const dashboard = target.kind === "custom" ? target.dashboard : undefined;
  const definition =
    target.kind === "custom"
      ? target.dashboard.definition
      : (target.stored?.definition ?? OVERVIEW);
  const defaultName = `Copy of ${dashboard?.name ?? "Overview"}`.slice(0, MAX_NAME_CHARS);
  // Only a stored Overview can be reset, and a demo viewer never has one.
  const canReset = target.kind === "overview" && target.stored !== null && !local;

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
          {canReset ? (
            <DropdownMenuItem disabled={layoutBusy} onSelect={() => setDialog("reset")}>
              Reset to default
            </DropdownMenuItem>
          ) : null}
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
      {canReset ? (
        <ResetOverviewDialog
          org={org}
          project={project}
          open={dialog === "reset"}
          onOpenChange={(next) => setDialog(next ? "reset" : null)}
          onReset={() => setDialog(null)}
        />
      ) : null}
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
