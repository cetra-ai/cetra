import { Link, useMatchRoute } from "@tanstack/react-router";
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

export function SettingsSidebar({
  org,
  project,
  children,
}: {
  org: string;
  project?: { slug: string; role: OrgRole | undefined };
  children: ReactNode;
}) {
  const matchRoute = useMatchRoute();
  const me = $api.useQuery("get", "/api/auth/me");
  const role = me.data?.orgs.find((candidate) => candidate.slug === org)?.role;
  const [filter, setFilter] = useState("");
  const needle = filter.trim().toLowerCase();
  const projectItems = project
    ? visibleProjectNav(project.role).filter((item) => matchesFilter(item, needle))
    : [];
  const orgItems = visibleOrgNav(role).filter((item) => matchesFilter(item, needle));
  const entitlements = useOrgEntitlements(org);
  const usage = entitlements.query.data?.usage;
  const limits = entitlements.query.data?.limits;

  return (
    <SidebarProvider className="h-[calc(100dvh-40px)] overflow-hidden">
      <Sidebar collapsible="none" aria-label="Settings">
        <SidebarHeader>
          <SidebarMenu>
            <SidebarMenuItem>
              <SidebarMenuButton asChild>
                {project ? (
                  <Link to="/$org/$project/traces" params={{ org, project: project.slug }}>
                    <ArrowLeft />
                    <span>Back to project</span>
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
          <Input
            aria-label="Filter settings"
            placeholder="Filter settings"
            className="h-7 text-body"
            value={filter}
            onChange={(event) => setFilter(event.target.value)}
          />
        </SidebarHeader>
        <SidebarContent>
          {project && projectItems.length > 0 ? (
            <SidebarGroup>
              <SidebarGroupLabel>Project</SidebarGroupLabel>
              <SidebarGroupContent>
                <SidebarMenu aria-label="Project settings">
                  {projectItems.map((item) => (
                    <SidebarMenuItem key={item.to}>
                      <SidebarMenuButton
                        asChild
                        isActive={Boolean(
                          matchRoute({
                            to: item.to,
                            params: { org, project: project.slug },
                          }),
                        )}
                      >
                        <Link to={item.to} params={{ org, project: project.slug }}>
                          {item.label}
                        </Link>
                      </SidebarMenuButton>
                    </SidebarMenuItem>
                  ))}
                </SidebarMenu>
              </SidebarGroupContent>
            </SidebarGroup>
          ) : null}
          {orgItems.length > 0 ? (
            <SidebarGroup>
              <SidebarGroupLabel>Organization</SidebarGroupLabel>
              <SidebarGroupContent>
                <SidebarMenu aria-label="Organization settings">
                  {orgItems.map((item) => (
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
              </SidebarGroupContent>
            </SidebarGroup>
          ) : null}
          {projectItems.length === 0 && orgItems.length === 0 ? (
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
