import { Link, useMatchRoute } from "@tanstack/react-router";
import { cn } from "cn";
import {
  FlaskConical,
  Gauge,
  type LucideIcon,
  MessagesSquare,
  Settings,
  Table2,
  Waypoints,
} from "lucide-react";
import { Fragment, type ReactNode, useEffect, useState } from "react";
import { GenAiLogsHint } from "@/components/shell/genai-logs-hint";
import { NAV_SECTIONS, projectNav } from "@/components/shell/nav";
import { useProject } from "@/components/shell/project-context";
import { type RecentItem, useRecentItems } from "@/components/shell/recent-items";
import { SidebarAccount } from "@/components/shell/sidebar-account";
import { type NavBadge, type NavBadgeHref, useNavBadges } from "@/components/shell/use-nav-badges";
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
  SidebarMenuBadge,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarSeparator,
  SidebarTrigger,
  useSidebar,
} from "@/components/ui/sidebar";
import { TooltipProvider } from "@/components/ui/tooltip";
import { goKeyOf } from "@/lib/shortcuts";
import { STORAGE_PREFIX } from "@/product";

const SIDEBAR_KEY = `${STORAGE_PREFIX}sidebar`;
/** Below this width the sidebar starts collapsed. The shadcn primitive switches to a sheet below 768. */
const NARROW_WIDTH = 1024;

function readSidebarOpen(): boolean {
  try {
    return localStorage.getItem(SIDEBAR_KEY) !== "closed";
  } catch {
    return true;
  }
}

function writeSidebarOpen(open: boolean): void {
  try {
    localStorage.setItem(SIDEBAR_KEY, open ? "open" : "closed");
  } catch {
    // Private mode can throw. The choice still applies for this page.
  }
}

function isNarrowWindow(): boolean {
  return window.innerWidth < NARROW_WIDTH;
}

/** Whether the window is narrower than NARROW_WIDTH, updated on resize. */
function useNarrowViewport(): boolean {
  const [narrow, setNarrow] = useState(isNarrowWindow);
  useEffect(() => {
    const onResize = () => {
      setNarrow(isNarrowWindow());
    };
    onResize();
    window.addEventListener("resize", onResize);
    return () => {
      window.removeEventListener("resize", onResize);
    };
  }, []);
  return narrow;
}

const RECENT_ICONS: Record<RecentItem["kind"], LucideIcon> = {
  trace: Waypoints,
  session: MessagesSquare,
  evaluator: Gauge,
  dataset: Table2,
  experiment: FlaskConical,
};

/** The go key of a page, shown as two kbd elements: G, then the letter. */
function GoKeys({ to }: { to: string }) {
  const key = goKeyOf(to);
  if (key === undefined) {
    return null;
  }
  return (
    <span className="inline-flex items-center gap-0.5">
      <kbd data-slot="kbd" className="border border-background/30 px-1 font-mono text-caption">
        G
      </kbd>
      <kbd data-slot="kbd" className="border border-background/30 px-1 font-mono text-caption">
        {key.toUpperCase()}
      </kbd>
    </span>
  );
}

function badgeDotClass(badge: NavBadge): string {
  if (badge.kind === "dot") {
    return "bg-status-warn";
  }
  return badge.tone === "destructive" ? "bg-status-error" : "bg-muted-foreground";
}

/** The page's badge in expanded mode: the count, or a dot. Icon mode shows only the dot on the icon. */
function NavBadgeSlot({ badge }: { badge: NavBadge }) {
  if (badge.kind === "dot") {
    return (
      <SidebarMenuBadge aria-label={badge.label}>
        <span className="size-2 rounded-full bg-status-warn" />
      </SidebarMenuBadge>
    );
  }
  return (
    <SidebarMenuBadge
      aria-label={badge.label}
      className={cn(
        badge.tone === "destructive"
          ? "rounded-full bg-status-error text-white peer-hover/menu-button:text-white peer-data-active/menu-button:text-white"
          : "text-muted-foreground",
      )}
    >
      {badge.value}
    </SidebarMenuBadge>
  );
}

function NavSections({ org, project }: { org: string; project: string }) {
  const { isMobile } = useSidebar();
  const matchRoute = useMatchRoute();
  const badges = useNavBadges(org, project);
  const recent = useRecentItems(org, project, 5);

  return (
    <SidebarContent>
      {NAV_SECTIONS.map((section, index) => (
        <Fragment key={section}>
          {index > 0 ? (
            <SidebarSeparator className="hidden group-data-[collapsible=icon]:block" />
          ) : null}
          <SidebarGroup className="py-1">
            <SidebarGroupLabel>{section}</SidebarGroupLabel>
            <SidebarGroupContent>
              <SidebarMenu>
                {projectNav
                  .filter((item) => item.section === section)
                  .map((item) => {
                    const badge: NavBadge | undefined =
                      item.to in badges ? badges[item.to as NavBadgeHref] : undefined;
                    return (
                      <SidebarMenuItem key={item.to}>
                        <SidebarMenuButton
                          asChild
                          isActive={Boolean(
                            matchRoute({ to: item.to, params: { org, project }, fuzzy: true }),
                          )}
                          tooltip={{
                            hidden: isMobile,
                            children: (
                              <span className="flex flex-col gap-0.5">
                                <span className="flex items-center gap-2">
                                  <span>{item.label}</span>
                                  <GoKeys to={item.to} />
                                </span>
                                {badge ? <span className="opacity-80">{badge.label}</span> : null}
                              </span>
                            ),
                          }}
                        >
                          <Link
                            to={item.to}
                            params={{ org, project }}
                            className="relative"
                            state={(current) => ({ ...current, applyDefaultView: true })}
                          >
                            <item.icon />
                            <span>{item.label}</span>
                            {badge ? (
                              <span
                                aria-hidden="true"
                                className={cn(
                                  "absolute top-1 right-1 hidden size-1.5 rounded-full group-data-[collapsible=icon]:block",
                                  badgeDotClass(badge),
                                )}
                              />
                            ) : null}
                          </Link>
                        </SidebarMenuButton>
                        {badge ? <NavBadgeSlot badge={badge} /> : null}
                      </SidebarMenuItem>
                    );
                  })}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        </Fragment>
      ))}
      {recent.length > 0 ? (
        <SidebarGroup className="py-1 group-data-[collapsible=icon]:hidden">
          <SidebarGroupLabel>Recent</SidebarGroupLabel>
          <SidebarGroupContent>
            <SidebarMenu>
              {recent.map((item) => {
                const Icon = RECENT_ICONS[item.kind];
                return (
                  <SidebarMenuItem key={`${item.kind}:${item.id}`}>
                    <SidebarMenuButton
                      asChild
                      size="sm"
                      tooltip={{ hidden: isMobile, children: item.label }}
                    >
                      {recentLink(item, org, project, [
                        <Icon key="icon" />,
                        <span key="label">{item.label}</span>,
                      ])}
                    </SidebarMenuButton>
                  </SidebarMenuItem>
                );
              })}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      ) : null}
    </SidebarContent>
  );
}

/** The link of a recent item. It is returned as a Link element, so SidebarMenuButton's asChild can merge its props. */
function recentLink(item: RecentItem, org: string, project: string, children: ReactNode) {
  switch (item.kind) {
    case "trace":
      return (
        <Link to="/$org/$project/traces/$traceId" params={{ org, project, traceId: item.id }}>
          {children}
        </Link>
      );
    case "session":
      return (
        <Link to="/$org/$project/sessions/$sessionId" params={{ org, project, sessionId: item.id }}>
          {children}
        </Link>
      );
    case "evaluator":
      return (
        <Link
          to="/$org/$project/evaluators/$evaluatorId"
          params={{ org, project, evaluatorId: item.id }}
        >
          {children}
        </Link>
      );
    case "dataset":
      return (
        <Link to="/$org/$project/datasets/$datasetId" params={{ org, project, datasetId: item.id }}>
          {children}
        </Link>
      );
    case "experiment":
      return (
        <Link
          to="/$org/$project/experiments"
          params={{ org, project }}
          search={{ experiment: item.id }}
        >
          {children}
        </Link>
      );
  }
}

export function ProjectSidebar({ children }: { children: ReactNode }) {
  const { org, project } = useProject();
  const narrow = useNarrowViewport();
  const [storedOpen, setStoredOpen] = useState(readSidebarOpen);
  // The choice made while narrow. It is not stored, and it starts collapsed at each narrow entry.
  const [narrowOpen, setNarrowOpen] = useState(false);
  const [wasNarrow, setWasNarrow] = useState(narrow);
  if (narrow !== wasNarrow) {
    setWasNarrow(narrow);
    setNarrowOpen(false);
  }
  const open = narrow ? narrowOpen : storedOpen;

  return (
    <TooltipProvider>
      <SidebarProvider
        open={open}
        onOpenChange={(next) => {
          if (narrow) {
            setNarrowOpen(next);
            return;
          }
          setStoredOpen(next);
          writeSidebarOpen(next);
        }}
        className="h-[calc(100dvh-40px)] overflow-hidden"
      >
        <Sidebar collapsible="icon">
          <SidebarHeader>
            <SidebarTrigger />
          </SidebarHeader>
          <NavSections org={org} project={project} />
          <SidebarFooter>
            <SidebarMenu>
              <SidebarMenuItem>
                <SidebarMenuButton asChild tooltip="Settings">
                  <Link to="/$org/$project/settings/general" params={{ org, project }}>
                    <Settings />
                    <span>Settings</span>
                  </Link>
                </SidebarMenuButton>
              </SidebarMenuItem>
            </SidebarMenu>
            <SidebarAccount />
          </SidebarFooter>
        </Sidebar>
        <SidebarInset className="min-h-0 overflow-hidden">
          <div className="flex shrink-0 items-center border-b px-2 py-1 md:hidden">
            <SidebarTrigger />
          </div>
          <GenAiLogsHint />
          {children}
        </SidebarInset>
      </SidebarProvider>
    </TooltipProvider>
  );
}
