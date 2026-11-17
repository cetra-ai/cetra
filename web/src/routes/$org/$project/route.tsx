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
import { openTraceId, rememberTrace } from "@/components/command/recent-traces";
import { DashboardView } from "@/components/dashboard/dashboard-view";
import { ScoreDefinitionsProvider } from "@/components/score-badge";
import { isProjectSettingsPath } from "@/components/settings/settings-nav";
import { SettingsSidebar } from "@/components/settings/settings-sidebar";
import { ProjectProvider, useProject } from "@/components/shell/project-context";
import { ProjectSidebar } from "@/components/shell/project-sidebar";
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

function RememberOpenTrace() {
  const { org, project } = useProject();
  const pathname = useRouterState({ select: (state) => state.location.pathname });
  const search = useRouterState({ select: (state) => state.location.search });
  const id = openTraceId(pathname, search);
  const detail = useQuery({
    ...traceDetailQuery(org, project, id ?? "0".repeat(32)),
    enabled: id !== undefined,
  });

  useEffect(() => {
    if (!id) {
      return;
    }
    const name = detail.data?.trace.name;
    rememberTrace(org, project, { id, name: name && name.length > 0 ? name : id });
  }, [detail.data, id, org, project]);

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

  return (
    <ProjectProvider value={{ org, project, ref: projectRef(org, project), data }}>
      <ScoreDefinitionsProvider>
        <RememberOpenTrace />
        {inSettings ? (
          <SettingsSidebar org={org} project={{ slug: project, role: data.role }}>
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
