import { Link, useMatchRoute, useNavigate, useRouterState } from "@tanstack/react-router";
import { ArrowLeft } from "lucide-react";
import { type ReactNode, useState } from "react";
import { $api } from "@/api/client";
import { UsageMeter } from "@/components/plan/usage-meter";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { visibleOrgNav, visibleProjectNav } from "@/components/settings/settings-home";
import { Input } from "@/components/ui/input";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarInset,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarTrigger,
} from "@/components/ui/sidebar";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { readLastProject } from "@/lib/last-project";
import type { OrgRole } from "@/lib/roles";

/** True when the filter is empty, or the label, description or a keyword contains it. */
function matchesFilter(
  item: { label: string; description: string; keywords: readonly string[] },
  needle: string,
): boolean {
  if (needle === "") {
    return true;
  }
  return [item.label, item.description, ...item.keywords].some((text) =>
    text.toLowerCase().includes(needle),
  );
}

type ProjectNavItems = ReturnType<typeof visibleProjectNav>;
type OrgNavItems = ReturnType<typeof visibleOrgNav>;

function ProjectNavList({
  org,
  slug,
  items,
}: {
  org: string;
  slug: string;
  items: ProjectNavItems;
}) {
  const matchRoute = useMatchRoute();
  return (
    <SidebarMenu aria-label="Project settings">
      {items.map((item) => (
        <SidebarMenuItem key={item.to}>
          <SidebarMenuButton
            asChild
            isActive={Boolean(matchRoute({ to: item.to, params: { org, project: slug } }))}
          >
            <Link to={item.to} params={{ org, project: slug }}>
              {item.label}
            </Link>
          </SidebarMenuButton>
        </SidebarMenuItem>
      ))}
    </SidebarMenu>
  );
}

function OrgNavList({ org, items }: { org: string; items: OrgNavItems }) {
  const matchRoute = useMatchRoute();
  return (
    <SidebarMenu aria-label="Organization settings">
      {items.map((item) => (
        <SidebarMenuItem key={item.to}>
          <SidebarMenuButton
            asChild
            isActive={Boolean(matchRoute({ to: item.to, params: { org } }))}
          >
            <Link to={item.to} params={{ org }}>
              {item.label}
            </Link>
          </SidebarMenuButton>
        </SidebarMenuItem>
      ))}
    </SidebarMenu>
  );
}

/** A group of nav links; the label is shown only while the filter groups both scopes. */
function NavGroup({ label, children }: { label?: string; children: ReactNode }) {
  return (
    <SidebarGroup>
      {label ? <SidebarGroupLabel>{label}</SidebarGroupLabel> : null}
      <SidebarGroupContent>{children}</SidebarGroupContent>
    </SidebarGroup>
  );
}

export function SettingsSidebar({
  org,
  project,
  children,
}: {
  org: string;
  project?: { slug: string; name: string; role: OrgRole | undefined };
  children: ReactNode;
}) {
  const navigate = useNavigate();
  // Subscribing to the location re-reads the remembered project after every navigation.
  useRouterState({ select: (state) => state.location.pathname });
  const remembered = readLastProject(org);
  const me = $api.useQuery("get", "/api/auth/me");
  const orgRole = me.data?.orgs.find((candidate) => candidate.slug === org)?.role;
  const [filter, setFilter] = useState("");
  const needle = filter.trim().toLowerCase();

  // The project the project scope links to: the open one, else the remembered one. Only the
  // open project's role is known; for a remembered project the organization role stands in.
  const projectSlug = project?.slug ?? remembered?.slug;
  const projectName = project?.name ?? remembered?.name;
  const projectRole = project ? project.role : orgRole;
  const projectMatches =
    projectSlug === undefined
      ? []
      : visibleProjectNav(projectRole).filter((item) => matchesFilter(item, needle));
  const orgMatches = visibleOrgNav(orgRole).filter((item) => matchesFilter(item, needle));
  // Without a filter only the current scope is listed; with one, both scopes are searched.
  const showProject = project !== undefined || needle !== "";
  const showOrg = project === undefined || needle !== "";
  const shownCount = (showProject ? projectMatches.length : 0) + (showOrg ? orgMatches.length : 0);
  const groupLabel = (name: string) => (needle !== "" ? name : undefined);

  const entitlements = useOrgEntitlements(org);
  const usage = entitlements.query.data?.usage;
  const limits = entitlements.query.data?.limits;

  const backTo = project ?? remembered;

  return (
    <SidebarProvider className="h-[calc(100dvh-40px)] overflow-hidden">
      <Sidebar collapsible="none" aria-label="Settings">
        <SidebarHeader>
          <SidebarMenu>
            <SidebarMenuItem>
              <SidebarMenuButton asChild>
                {backTo ? (
                  <Link to="/$org/$project/traces" params={{ org, project: backTo.slug }}>
                    <ArrowLeft />
                    <span>Back to {backTo.name}</span>
                  </Link>
                ) : (
                  <Link to="/$org" params={{ org }}>
                    <ArrowLeft />
                    <span>Back to projects</span>
                  </Link>
                )}
              </SidebarMenuButton>
            </SidebarMenuItem>
          </SidebarMenu>
          {projectSlug === undefined || projectName === undefined ? (
            <p className="px-2 py-1 text-ui font-medium">Organization</p>
          ) : (
            <Tabs
              // Manual: arrowing across the switch moves focus; Enter or a click changes scope.
              activationMode="manual"
              value={project ? "project" : "org"}
              onValueChange={(next) => {
                if (next === "org") {
                  void navigate({ to: "/$org/settings", params: { org } });
                } else {
                  void navigate({
                    to: "/$org/$project/settings",
                    params: { org, project: projectSlug },
                  });
                }
              }}
            >
              <TabsList aria-label="Settings scope" className="grid w-full grid-cols-2">
                <TabsTrigger value="project" className="min-w-0 truncate">
                  {projectName.length > 0 ? projectName : "Project"}
                </TabsTrigger>
                <TabsTrigger value="org" className="min-w-0 truncate">
                  Organization
                </TabsTrigger>
              </TabsList>
            </Tabs>
          )}
          <Input
            aria-label="Filter settings"
            placeholder="Filter settings"
            className="h-7 text-body"
            value={filter}
            onChange={(event) => setFilter(event.target.value)}
          />
        </SidebarHeader>
        <SidebarContent>
          {showProject && projectSlug !== undefined && projectMatches.length > 0 ? (
            <NavGroup label={groupLabel("Project")}>
              <ProjectNavList org={org} slug={projectSlug} items={projectMatches} />
            </NavGroup>
          ) : null}
          {showOrg && orgMatches.length > 0 ? (
            <NavGroup label={groupLabel("Organization")}>
              <OrgNavList org={org} items={orgMatches} />
            </NavGroup>
          ) : null}
          {shownCount === 0 ? (
            <p className="px-2 py-1 text-body text-muted-foreground">No settings match</p>
          ) : null}
        </SidebarContent>
        {usage && limits ? (
          <SidebarFooter>
            <Link
              to="/$org/settings/plan"
              params={{ org }}
              className="flex flex-col gap-1.5 rounded-md p-2 hover:bg-sidebar-accent"
            >
              <span className="text-caption font-medium text-muted-foreground">Usage</span>
              <UsageMeter
                variant="compact"
                units={usage.units}
                resetsAt={usage.resets_at}
                included={limits.units_included}
                cap={limits.units_cap}
              />
            </Link>
          </SidebarFooter>
        ) : null}
      </Sidebar>
      <SidebarInset className="min-h-0 overflow-hidden">
        <div className="flex shrink-0 items-center border-b px-2 py-1 md:hidden">
          <SidebarTrigger aria-label="Open navigation" />
        </div>
        {children}
      </SidebarInset>
    </SidebarProvider>
  );
}
