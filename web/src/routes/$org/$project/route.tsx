import { useQuery } from "@tanstack/react-query";
import {
  createFileRoute,
  notFound,
  Outlet,
  useMatch,
  useRouterState,
} from "@tanstack/react-router";
import { Activity, useEffect, useRef } from "react";
import { $api } from "@/api/client";
import { problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { projectInit, projectQuery, traceDetailQuery } from "@/api/queries";
import { datasetQuery } from "@/api/queries-datasets";
import { evaluatorQuery } from "@/api/queries-evaluation";
import { experimentQuery } from "@/api/queries-experiments";
import { openTraceId } from "@/components/command/recent-traces";
import { DashboardView } from "@/components/dashboard/dashboard-view";
import { ScoreDefinitionsProvider } from "@/components/score-badge";
import { isProjectSettingsPath } from "@/components/settings/settings-nav";
import { SettingsSidebar } from "@/components/settings/settings-sidebar";
import { ProjectProvider, useProject } from "@/components/shell/project-context";
import { ProjectSidebar } from "@/components/shell/project-sidebar";
import { type RecentKind, rememberItem } from "@/components/shell/recent-items";
import { rememberLastProject } from "@/lib/last-project";
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
 * shown, visible again (with its charts already drawn) when the user comes back.
 */
function KeptDashboard() {
  const match = useMatch({ from: "/$org/$project/dashboard/", shouldThrow: false });
  const kept = useRef<DashboardSearch | undefined>(undefined);
  if (match) {
    kept.current = match.search;
  }
  if (!kept.current) {
    return null;
  }
  return (
    <Activity mode={match ? "visible" : "hidden"}>
      <DashboardView search={kept.current} />
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

  useEffect(() => {
    rememberLastProject(org, { slug: project, name: data.name });
  }, [org, project, data.name]);

  return (
    <ProjectProvider value={{ org, project, ref: projectRef(org, project), data }}>
      <ScoreDefinitionsProvider>
        <RememberOpenItem />
        {inSettings ? (
          <SettingsSidebar org={org} project={{ slug: project, name: data.name, role: data.role }}>
            <Outlet />
          </SettingsSidebar>
        ) : (
          <ProjectSidebar>
            <Outlet />
            <KeptDashboard />
          </ProjectSidebar>
        )}
      </ScoreDefinitionsProvider>
    </ProjectProvider>
  );
}
