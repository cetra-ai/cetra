import { useQueryClient } from "@tanstack/react-query";
import { MoreHorizontal } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";
import type { Layout } from "react-grid-layout";
import { dashboardQuery, isStaleDashboard, useUpdateDashboard } from "@/api/dashboards";
import { problemMessage, problemOf } from "@/api/problem";
import { UnsavedChangesGuard } from "@/components/settings/unsaved-changes";
import { useProject } from "@/components/shell/project-context";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useIsMobile } from "@/hooks/use-mobile";
import {
  bottomOf,
  issueWidgetIndex,
  issueWithin,
  newWidgetId,
  sameDefinition,
  withLayouts,
} from "@/lib/dashboards/draft";
import { OVERVIEW } from "@/lib/dashboards/overview";
import type { Dashboard, Definition, Widget } from "@/lib/dashboards/types";
import { type FieldIssue, validateDefinition } from "@/lib/dashboards/validate";
import { canManageDashboards } from "@/lib/roles";
import { DashboardMenu, DashboardSwitcher } from "./dashboard-header";
import type { WidgetContext } from "./widget";
import { HeadingDialog, WidgetEditor } from "./widget-editor";
import { WidgetGrid } from "./widget-grid";

/** A dashboard's widgets, the most a definition holds (spec 04 § Dashboards). */
const MAX_WIDGETS = 50;
/** A new chart's size: half the grid, seven rows. */
const NEW_CHART = { w: 6, h: 7 } as const;

/**
 * An edit session: a draft of the definition, started from the version it was read at. The
 * save names that version, so a change made elsewhere since then is a `409`, not an overwrite.
 */
type Session = {
  version: number;
  base: Definition;
  draft: Definition;
  /** Client or server rule failures, by path. */
  issues: FieldIssue[];
  /** A failed save that is not about a field. */
  message?: string;
  /** The save found a newer version. */
  conflict: boolean;
};

/**
 * The dialog that is open. `index` is the widget's place in the draft; for a new widget it is
 * where it will be appended.
 */
type Open =
  | { kind: "widget"; index: number; base: Widget }
  | { kind: "heading"; index?: number; title: string }
  | null;

/**
 * The grid of the open dashboard, with its header. Members on a custom dashboard can edit it:
 * the draft is kept here until it is saved or dropped. The Overview is read only.
 */
export function DashboardBody({
  dashboard,
  active,
  context,
  renderWidget,
}: {
  /** The custom dashboard, or undefined for the Overview. */
  dashboard: Dashboard | undefined;
  /** Whether the dashboard is the one on screen. A hidden dashboard drops its draft. */
  active: boolean;
  context: WidgetContext;
  renderWidget: (widget: Widget) => ReactNode;
}) {
  const { org, project, data } = useProject();
  const queryClient = useQueryClient();
  const narrow = useIsMobile();
  const update = useUpdateDashboard(org, project, dashboard?.id ?? "");
  const [session, setSession] = useState<Session | null>(null);
  const [open, setOpen] = useState<Open>(null);

  // A hidden dashboard keeps its state, so leaving it drops the draft: the effect runs when it
  // is shown again.
  useEffect(() => {
    if (active) {
      setSession(null);
      setOpen(null);
    }
  }, [active]);

  const canEdit = dashboard !== undefined && canManageDashboards(data.role) && !narrow;
  const editing = canEdit && session !== null;
  const draft = editing ? session.draft : undefined;
  const shown: Definition = draft ?? dashboard?.definition ?? OVERVIEW;
  const dirty = session !== null && editing && !sameDefinition(session.draft, session.base);

  function changeDraft(change: (current: Definition) => Definition) {
    setSession((current) =>
      current === null
        ? current
        : { ...current, draft: change(current.draft), issues: [], message: undefined },
    );
  }

  function applyLayout(layout: Layout) {
    setSession((current) => {
      if (current === null) {
        return current;
      }
      const widgets = withLayouts(current.draft.widgets, layout);
      if (widgets === current.draft.widgets) {
        return current;
      }
      return { ...current, draft: { widgets }, issues: [], message: undefined };
    });
  }

  function startEditing() {
    if (dashboard === undefined) {
      return;
    }
    setSession({
      version: dashboard.version,
      base: dashboard.definition,
      draft: dashboard.definition,
      issues: [],
      conflict: false,
    });
  }

  function cancelEditing() {
    setSession(null);
    setOpen(null);
  }

  function applyWidget(widget: Widget, index: number) {
    changeDraft((current) => ({ widgets: putWidget(current.widgets, index, widget) }));
  }

  function removeWidget(index: number) {
    changeDraft((current) => ({
      widgets: current.widgets.filter((_, at) => at !== index),
    }));
  }

  function duplicateWidget(index: number) {
    changeDraft((current) => {
      const source = current.widgets[index];
      if (source === undefined) {
        return current;
      }
      const copy: Widget = {
        ...source,
        id: newWidgetId(),
        layout: {
          x: source.layout.x,
          y: bottomOf(current.widgets),
          w: source.layout.w,
          h: source.layout.h,
        },
      };
      return { widgets: [...current.widgets, copy] };
    });
  }

  function applyHeading(title: string, index: number | undefined) {
    changeDraft((current) => {
      if (index === undefined) {
        const heading: Widget = {
          id: newWidgetId(),
          title,
          kind: "heading",
          layout: { x: 0, y: bottomOf(current.widgets), w: 12, h: 1 },
        };
        return { widgets: [...current.widgets, heading] };
      }
      const existing = current.widgets[index];
      if (existing === undefined) {
        return current;
      }
      return { widgets: putWidget(current.widgets, index, { ...existing, title }) };
    });
  }

  function openNewWidget() {
    if (session === null) {
      return;
    }
    const index = session.draft.widgets.length;
    setOpen({
      kind: "widget",
      index,
      base: {
        id: newWidgetId(),
        title: "",
        kind: "chart",
        layout: { x: 0, y: bottomOf(session.draft.widgets), ...NEW_CHART },
      },
    });
  }

  function openWidget(index: number) {
    const widget = session?.draft.widgets[index];
    if (widget === undefined) {
      return;
    }
    setOpen(
      widget.kind === "heading"
        ? { kind: "heading", index, title: widget.title }
        : { kind: "widget", index, base: widget },
    );
  }

  async function save() {
    if (session === null || dashboard === undefined) {
      return;
    }
    const issues = validateDefinition(session.draft);
    if (issues.length > 0) {
      setSession({ ...session, issues, message: undefined });
      return;
    }
    try {
      await update.mutateAsync({ version: session.version, definition: session.draft });
      setSession(null);
    } catch (error) {
      const problem = problemOf(error);
      setSession((current) => {
        if (current === null) {
          return current;
        }
        if (isStaleDashboard(error)) {
          return { ...current, conflict: true };
        }
        if (problem?.status === 400) {
          return { ...current, issues: problem.errors ?? [], message: undefined };
        }
        return { ...current, message: problemMessage(error, "Could not save the dashboard.") };
      });
    }
  }

  function reload() {
    setSession(null);
    setOpen(null);
    if (dashboard !== undefined) {
      void queryClient.invalidateQueries({
        queryKey: dashboardQuery(org, project, dashboard.id).queryKey,
      });
    }
  }

  // Issues about a widget sit on its cell; the others, and a failed save, sit at the top.
  const issues = session?.issues ?? [];
  const topIssues = issues.filter((issue) => {
    const index = issueWidgetIndex(issue.field);
    return index === undefined || index >= shown.widgets.length;
  });
  const full = (draft?.widgets.length ?? 0) >= MAX_WIDGETS;

  return (
    <>
      <div className="flex flex-wrap items-center gap-2 pb-3">
        <DashboardSwitcher dashboard={dashboard} />
        <div className="ml-auto flex flex-wrap items-center gap-2">
          {editing ? (
            <>
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={full}
                title={full ? `A dashboard holds at most ${MAX_WIDGETS} widgets.` : undefined}
                onClick={openNewWidget}
              >
                Add widget
              </Button>
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={() => setOpen({ kind: "heading", title: "" })}
              >
                Add heading
              </Button>
              <Button type="button" variant="outline" size="sm" onClick={cancelEditing}>
                Cancel
              </Button>
              <Button
                type="button"
                size="sm"
                disabled={update.isPending}
                onClick={() => void save()}
              >
                Save
              </Button>
            </>
          ) : canEdit ? (
            <Button type="button" variant="outline" size="sm" onClick={startEditing}>
              Edit
            </Button>
          ) : null}
          {editing ? null : <DashboardMenu dashboard={dashboard} />}
        </div>
      </div>

      {editing && (topIssues.length > 0 || session.message) ? (
        <div role="alert" className="flex flex-col gap-1 pb-3 text-caption text-destructive">
          {session.message ? <p>{session.message}</p> : null}
          {topIssues.map((issue) => (
            <p key={`${issue.field}:${issue.message}`}>
              {issueWithin(issue.field)}: {issue.message}
            </p>
          ))}
        </div>
      ) : null}

      <WidgetGrid
        widgets={shown.widgets}
        editing={editing}
        onLayoutChange={editing ? applyLayout : undefined}
        renderCell={(widget) => {
          if (!editing) {
            return renderWidget(widget);
          }
          const index = shown.widgets.findIndex((item) => item.id === widget.id);
          const own = issues.filter((issue) => issueWidgetIndex(issue.field) === index);
          return (
            <>
              {renderWidget(widget)}
              <WidgetMenu
                title={widget.title}
                onEdit={() => openWidget(index)}
                onDuplicate={() => duplicateWidget(index)}
                onRemove={() => removeWidget(index)}
              />
              {own.length > 0 ? (
                <ul
                  role="alert"
                  className="absolute inset-x-1 bottom-1 z-10 flex flex-col gap-0.5 rounded-md bg-background/95 px-2 py-1 text-caption text-destructive shadow-xs"
                >
                  {own.map((issue) => (
                    <li key={`${issue.field}:${issue.message}`}>
                      {issueWithin(issue.field)}: {issue.message}
                    </li>
                  ))}
                </ul>
              ) : null}
            </>
          );
        }}
      />

      {draft !== undefined && open?.kind === "widget" ? (
        <WidgetEditor
          open
          onOpenChange={(next) => {
            if (!next) setOpen(null);
          }}
          base={open.base}
          draft={draft}
          index={open.index}
          context={context}
          onApply={(widget) => {
            applyWidget(widget, open.index);
            setOpen(null);
          }}
        />
      ) : null}
      {open?.kind === "heading" ? (
        <HeadingDialog
          open
          onOpenChange={(next) => {
            if (!next) setOpen(null);
          }}
          initial={open.title}
          onApply={(title) => {
            applyHeading(title, open.index);
            setOpen(null);
          }}
        />
      ) : null}

      <AlertDialog
        open={session?.conflict === true}
        onOpenChange={(next) => {
          if (!next) {
            setSession((current) => (current === null ? current : { ...current, conflict: false }));
          }
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Someone else changed this dashboard.</AlertDialogTitle>
            <AlertDialogDescription>
              Reload to see their version. Your edits are dropped.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Keep editing</AlertDialogCancel>
            <AlertDialogAction variant="destructive" onClick={reload}>
              Reload
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      <UnsavedChangesGuard dirty={dirty} />
    </>
  );
}

/** The widgets with `widget` at `index`, or appended when `index` is past the end. */
function putWidget(widgets: Widget[], index: number, widget: Widget): Widget[] {
  return index < widgets.length
    ? widgets.map((item, at) => (at === index ? widget : item))
    : [...widgets, widget];
}

/** The "⋯" menu of a widget in edit mode: Edit, Duplicate, Remove. */
function WidgetMenu({
  title,
  onEdit,
  onDuplicate,
  onRemove,
}: {
  title: string;
  onEdit: () => void;
  onDuplicate: () => void;
  onRemove: () => void;
}) {
  return (
    <div className="absolute top-1 right-1 z-10">
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button
            type="button"
            variant="outline"
            size="icon-xs"
            aria-label={`Actions for ${title === "" ? "widget" : title}`}
          >
            <MoreHorizontal />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="min-w-36">
          <DropdownMenuItem onSelect={onEdit}>Edit</DropdownMenuItem>
          <DropdownMenuItem onSelect={onDuplicate}>Duplicate</DropdownMenuItem>
          <DropdownMenuItem variant="destructive" onSelect={onRemove}>
            Remove
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}
