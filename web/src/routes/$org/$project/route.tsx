import { useQuery } from "@tanstack/react-query";
import {
  createFileRoute,
  notFound,
  Outlet,
  useMatch,
  useRouterState,
} from "@tanstack/react-router";
import { Activity, useEffect, useRef } from "react";
import { useGroupRef } from "react-resizable-panels";
import { $api } from "@/api/client";
import { problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { projectInit, projectQuery, traceDetailQuery } from "@/api/queries";
import { datasetQuery } from "@/api/queries-datasets";
import { evaluatorQuery } from "@/api/queries-evaluation";
import { experimentQuery } from "@/api/queries-experiments";
import { AssistantPanel } from "@/components/assistant/assistant-panel";
import { openTraceId } from "@/components/command/recent-traces";
import { useShortcut } from "@/components/command/use-shortcut";
import { DashboardView } from "@/components/dashboard/dashboard-view";
import { ScoreDefinitionsProvider } from "@/components/score-badge";
import { isProjectSettingsPath } from "@/components/settings/settings-nav";
import { SettingsSidebar } from "@/components/settings/settings-sidebar";
import { ProjectProvider, useProject } from "@/components/shell/project-context";
import { ProjectSidebar } from "@/components/shell/project-sidebar";
import { type RecentKind, rememberItem } from "@/components/shell/recent-items";
import { ResizableHandle, ResizablePanel, ResizablePanelGroup } from "@/components/ui/resizable";
import { Sheet, SheetContent, SheetTitle } from "@/components/ui/sheet";
import { useNarrowViewport } from "@/hooks/use-narrow-viewport";
import {
  readPanelSize,
  setPanelOpen,
  togglePanel,
  usePanelOpen,
  writePanelSize,
} from "@/lib/assistant/panel";
import { rememberLastProject } from "@/lib/last-project";
import { canUseAssistant } from "@/lib/roles";
import type { DashboardSearch } from "@/lib/search/dashboard";

export const Route = createFileRoute("/$org/$project")({
  loader: async ({ context, params }) => {
    try {
      await context.queryClient.ensureQueryData(projectQuery(params.org, params.project));
    } catch (error) {
      if (problemOf(error)?.status === 404) {
        throw notFound();
      }
      throw error;
    }
  },
  component: ProjectLayout,
});

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** Records an open item in the recent list, labelled with its name once the name is known. */
function useRememberOpen(kind: RecentKind, id: string | undefined, name: string | undefined) {
  const { org, project } = useProject();
  useEffect(() => {
    if (id === undefined) {
      return;
    }
    rememberItem(org, project, { kind, id, label: name && name.length > 0 ? name : id });
  }, [kind, id, name, org, project]);
}

/**
 * Records whichever entity this page opens (trace, session, evaluator, dataset, experiment).
 * Names come from the same queries the pages use, enabled only for the open kind.
 */
function RememberOpenItem() {
  const { org, project } = useProject();
  const pathname = useRouterState({ select: (state) => state.location.pathname });
  const search = useRouterState({ select: (state) => state.location.search });

  const traceId = openTraceId(pathname, search);
  const trace = useQuery({
    ...traceDetailQuery(org, project, traceId ?? "0".repeat(32)),
    enabled: traceId !== undefined,
  });
  useRememberOpen("trace", traceId, trace.data?.trace.name);

  // The sessions list has no split: a session is only opened on its own page.
  const session = useMatch({ from: "/$org/$project/sessions/$sessionId", shouldThrow: false });
  useRememberOpen("session", session?.params.sessionId, undefined);

  const evaluator = useMatch({
    from: "/$org/$project/evaluators/$evaluatorId",
    shouldThrow: false,
  });
  const evaluatorId = UUID.test(evaluator?.params.evaluatorId ?? "")
    ? evaluator?.params.evaluatorId
    : undefined;
  const evaluatorData = useQuery({
    ...evaluatorQuery(org, project, evaluatorId ?? ""),
    enabled: evaluatorId !== undefined,
  });
  useRememberOpen("evaluator", evaluatorId, evaluatorData.data?.name);

  const dataset = useMatch({ from: "/$org/$project/datasets/$datasetId", shouldThrow: false });
  const datasetId = UUID.test(dataset?.params.datasetId ?? "")
    ? dataset?.params.datasetId
    : undefined;
  const datasetData = useQuery({
    ...datasetQuery(org, project, datasetId ?? ""),
    enabled: datasetId !== undefined,
  });
  useRememberOpen("dataset", datasetId, datasetData.data?.name);

  // The experiments list opens one experiment in its panel through `?experiment=`, which the
  // route's search parser already checks as a UUID.
  const experiments = useMatch({ from: "/$org/$project/experiments/", shouldThrow: false });
  const experimentId = experiments?.search.experiment;
  const experimentData = useQuery({
    ...experimentQuery(org, project, experimentId ?? ""),
    enabled: experimentId !== undefined,
  });
  useRememberOpen("experiment", experimentId, experimentData.data?.name);

  return null;
}

/**
 * Keeps the dashboard mounted once it has been opened: hidden while another project page is
 * shown, visible again (with its charts already drawn) when the user comes back. The Overview
 * and a custom dashboard share this view; switching dashboards remounts it.
 */
function KeptDashboard() {
  const { org, project } = useProject();
  const overview = useMatch({ from: "/$org/$project/dashboard/", shouldThrow: false });
  const custom = useMatch({ from: "/$org/$project/dashboard/$dashboardId", shouldThrow: false });
  const match = overview ?? custom;
  const kept = useRef<{ search: DashboardSearch; dashboardId?: string } | undefined>(undefined);
  if (match) {
    kept.current = { search: match.search, dashboardId: custom?.params.dashboardId };
  }
  if (!kept.current) {
    return null;
  }
  const { search, dashboardId } = kept.current;
  return (
    <Activity mode={match ? "visible" : "hidden"}>
      <DashboardView
        key={`${org}/${project}/${dashboardId ?? "overview"}`}
        search={search}
        dashboardId={dashboardId}
        active={match !== undefined}
      />
    </Activity>
  );
}

function ProjectLayout() {
  const { org, project } = Route.useParams();
  const { data } = $api.useSuspenseQuery(
    "get",
    "/api/v1/projects/{project}",
    projectInit(org, project),
  );
  const pathname = useRouterState({ select: (state) => state.location.pathname });
  const inSettings = isProjectSettingsPath(pathname, org, project);
  const ref = projectRef(org, project);
  const narrow = useNarrowViewport();
  const panelOpen = usePanelOpen();
  const canAssistant = canUseAssistant(data.role);
  // Beside the page on a wide window; a full-width sheet on a narrow one (D-AS-26).
  const wide = panelOpen && canAssistant && !narrow;
  const sheet = panelOpen && canAssistant && narrow;
  const groupRef = useGroupRef();

  useEffect(() => {
    rememberLastProject(org, { slug: project, name: data.name });
  }, [org, project, data.name]);

  useShortcut("assistant", () => {
    if (canAssistant) {
      togglePanel();
    }
  });

  // The group and both panels stay mounted in every state, so the page never remounts when the
  // panel opens or closes. Sizes are percentages.
  useEffect(() => {
    groupRef.current?.setLayout(
      wide
        ? { page: 100 - readPanelSize(), assistant: readPanelSize() }
        : { page: 100, assistant: 0 },
    );
  }, [groupRef, wide]);

  return (
    <ProjectProvider value={{ org, project, ref, data }}>
      <ScoreDefinitionsProvider>
        <RememberOpenItem />
        <ResizablePanelGroup
          id="assistant-layout"
          groupRef={groupRef}
          orientation="horizontal"
          disabled={!wide}
          className="min-h-0 flex-1"
          defaultLayout={{ page: 100, assistant: 0 }}
          onLayoutChanged={(layout, meta) => {
            const size = layout.assistant;
            if (wide && meta.isUserInteraction && size !== undefined) {
              writePanelSize(size);
            }
          }}
        >
          <ResizablePanel
            id="page"
            minSize={wide ? "50%" : "100%"}
            className="relative"
            style={{ overflow: "hidden" }}
          >
            {inSettings ? (
              <SettingsSidebar
                org={org}
                project={{ slug: project, name: data.name, role: data.role }}
              >
                <Outlet />
              </SettingsSidebar>
            ) : (
              <ProjectSidebar>
                <Outlet />
                <KeptDashboard />
              </ProjectSidebar>
            )}
          </ResizablePanel>
          {wide ? <ResizableHandle withHandle /> : null}
          <ResizablePanel
            id="assistant"
            minSize={wide ? "20%" : "0%"}
            maxSize={wide ? "50%" : "0%"}
            style={{ overflow: "hidden" }}
          >
            {wide ? <AssistantPanel key={ref} /> : null}
          </ResizablePanel>
        </ResizablePanelGroup>
        {sheet ? (
          <Sheet
            open
            onOpenChange={(open) => {
              setPanelOpen(open);
            }}
          >
            <SheetContent side="right" showCloseButton={false} className="w-full sm:max-w-none">
              <SheetTitle className="sr-only">Assistant</SheetTitle>
              <AssistantPanel key={ref} />
            </SheetContent>
          </Sheet>
        ) : null}
      </ScoreDefinitionsProvider>
    </ProjectProvider>
  );
}
