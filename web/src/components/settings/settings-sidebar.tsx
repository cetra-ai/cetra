import { Link, useMatchRoute } from "@tanstack/react-router";
import { ArrowLeft } from "lucide-react";
import type { ReactNode } from "react";
import { $api } from "@/api/client";
import { orgSettingsNav, projectSettingsNav } from "@/components/settings/settings-nav";
import {
  Sidebar,
  SidebarContent,
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
import { atLeast, type OrgRole } from "@/lib/roles";

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
        </SidebarHeader>
        <SidebarContent>
          {project ? (
            <SidebarGroup>
              <SidebarGroupLabel>Project</SidebarGroupLabel>
              <SidebarGroupContent>
                <SidebarMenu aria-label="Project settings">
                  {projectSettingsNav
                    .filter((item) => !item.adminOnly || atLeast(project.role, "admin"))
                    .map((item) => (
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
          <SidebarGroup>
            <SidebarGroupLabel>Organization</SidebarGroupLabel>
            <SidebarGroupContent>
              <SidebarMenu aria-label="Organization settings">
                {orgSettingsNav
                  .filter((item) => !item.ownerOnly || role === "owner")
                  .map((item) => (
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
        </SidebarContent>
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
