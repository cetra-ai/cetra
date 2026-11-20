import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import type { LucideIcon } from "lucide-react";
import type { ReactNode } from "react";
import { $api } from "@/api/client";
import { orgAssistantInit } from "@/api/queries-assistant";
import { channelsQuery } from "@/api/queries-alerts";
import { scoreDefinitionsInit } from "@/api/queries-evaluation";
import { orgPricesInit, projectPricesInit } from "@/api/queries-prices";
import {
  apiKeysInit,
  connectionsInit,
  deletionsInit,
  orgInvitesInit,
  orgMembersInit,
  orgSsoQuery,
  projectMembersInit,
} from "@/api/queries-settings";
import { usagePercent } from "@/components/plan/usage-meter";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { orgSettingsNav, projectSettingsNav } from "@/components/settings/settings-nav";
import { SettingsPage } from "@/components/settings/settings-page";
import { useProject } from "@/components/shell/project-context";
import { Skeleton } from "@/components/ui/skeleton";
import { formatRelative } from "@/lib/format";
import { atLeast, type OrgRole } from "@/lib/roles";

/** The org settings the role may open, as the sidebar and the palette list them. */
export function visibleOrgNav(role: OrgRole | undefined) {
  return orgSettingsNav.filter((item) => !item.ownerOnly || role === "owner");
}

/** The project settings the role may open, as the sidebar and the palette list them. */
export function visibleProjectNav(role: OrgRole | undefined) {
  return projectSettingsNav.filter((item) => !item.adminOnly || atLeast(role, "admin"));
}

const CARD =
  "flex flex-col gap-2 rounded-lg border bg-card p-4 text-card-foreground transition-colors hover:bg-muted/40 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring";

const INFINITE = {
  pageParamName: "cursor",
  initialPageParam: null,
  getNextPageParam: (page: { next_cursor?: string | null }) => page.next_cursor ?? undefined,
};

function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

/** The count of a paged list, with "+" while more pages exist. */
function paged(count: number, more: boolean, one: string, many: string): string {
  return `${count}${more ? "+" : ""} ${count === 1 && !more ? one : many}`;
}

/** The count of a paged list as a number, with "+" while more pages exist. */
function pagedCount(count: number, more: boolean): string {
  return `${count}${more ? "+" : ""}`;
}

/** The newest RFC 3339 instant of the list, or undefined when none is set. */
function latest(times: (string | undefined)[]): string | undefined {
  let best: string | undefined;
  let bestMs = Number.NEGATIVE_INFINITY;
  for (const time of times) {
    const ms = time === undefined ? Number.NaN : Date.parse(time);
    if (!Number.isNaN(ms) && ms > bestMs) {
      best = time;
      bestMs = ms;
    }
  }
  return best;
}

function StatusLine({ children }: { children: ReactNode }) {
  return <p className="text-body text-muted-foreground">{children}</p>;
}

function ProjectRetentionStatus() {
  const { data } = useProject();
  const days = data.retention_days;
  return (
    <StatusLine>
      {typeof days === "number" ? `Retention ${days} days` : "Data kept forever"}
    </StatusLine>
  );
}

function ApiKeysStatus({ org, project }: { org: string; project: string }) {
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/api-keys",
    apiKeysInit(org, project),
    INFINITE,
  );
  if (query.isPending) {
    return <Skeleton className="h-4 w-40" />;
  }
  if (query.data === undefined) {
    return null;
  }
  const rows = query.data.pages.flatMap((page) => page.data);
  const lastUsed = latest(rows.map((row) => row.last_used_at));
  return (
    <StatusLine>
      {`${paged(rows.length, query.hasNextPage, "key", "keys")} · ${
        lastUsed === undefined ? "never used" : `last used ${formatRelative(lastUsed, Date.now())}`
      }`}
    </StatusLine>
  );
}

function ProjectMembersStatus({ org, project }: { org: string; project: string }) {
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/members",
    projectMembersInit(org, project),
    INFINITE,
  );
  if (query.isPending) {
    return <Skeleton className="h-4 w-24" />;
  }
  if (query.data === undefined) {
    return null;
  }
  const rows = query.data.pages.flatMap((page) => page.data);
  return <StatusLine>{paged(rows.length, query.hasNextPage, "member", "members")}</StatusLine>;
}

function ProjectScoresStatus({ org, project }: { org: string; project: string }) {
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/score-definitions",
    scoreDefinitionsInit(org, project, false),
    INFINITE,
  );
  if (query.isPending) {
    return <Skeleton className="h-4 w-24" />;
  }
  if (query.data === undefined) {
    return null;
  }
  const rows = query.data.pages.flatMap((page) => page.data);
  return <StatusLine>{`${pagedCount(rows.length, query.hasNextPage)} active`}</StatusLine>;
}

function ProjectPricesStatus({ org, project }: { org: string; project: string }) {
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/models",
    projectPricesInit(org, project, "project", ""),
    INFINITE,
  );
  if (query.isPending) {
    return <Skeleton className="h-4 w-24" />;
  }
  if (query.data === undefined) {
    return null;
  }
  const rows = query.data.pages.flatMap((page) => page.data);
  return (
    <StatusLine>{paged(rows.length, query.hasNextPage, "own price", "own prices")}</StatusLine>
  );
}

function ChannelsStatus({ org, project }: { org: string; project: string }) {
  const query = useQuery(channelsQuery(org, project));
  if (query.isPending) {
    return <Skeleton className="h-4 w-32" />;
  }
  if (query.data === undefined) {
    return null;
  }
  const channels = query.data.data;
  const failing = channels.filter((channel) => channel.last_delivery?.status === "failed").length;
  return (
    <StatusLine>
      {plural(channels.length, "channel", "channels")}
      {failing > 0 ? `, ${failing} failing` : ""}
    </StatusLine>
  );
}

const DELETION_LABEL = {
  resolving: "Resolving",
  pending: "Pending",
  applied: "Applied",
  failed: "Failed",
} as const;

function DeletionStatus({ org, project }: { org: string; project: string }) {
  const query = $api.useQuery(
    "get",
    "/api/v1/projects/{project}/deletions",
    deletionsInit(org, project),
  );
  if (query.isPending) {
    return <Skeleton className="h-4 w-40" />;
  }
  if (query.data === undefined) {
    return null;
  }
  const newest = query.data.data[0];
  if (newest === undefined) {
    return <StatusLine>No deletion requests</StatusLine>;
  }
  const label = DELETION_LABEL[newest.status];
  return <StatusLine>{`Last request: ${label}`}</StatusLine>;
}

function OrgSlugStatus({ org }: { org: string }) {
  return <StatusLine>{`Slug: ${org}`}</StatusLine>;
}

function OrgMembersStatus({ org, canInvite }: { org: string; canInvite: boolean }) {
  const members = $api.useInfiniteQuery(
    "get",
    "/api/v1/orgs/{org}/members",
    orgMembersInit(org),
    INFINITE,
  );
  // Invitations are an admin feature: a member would only get a 403 here.
  const invites = $api.useInfiniteQuery("get", "/api/v1/orgs/{org}/invites", orgInvitesInit(org), {
    ...INFINITE,
    enabled: canInvite,
  });
  if (members.isPending || (canInvite && invites.isPending)) {
    return <Skeleton className="h-4 w-40" />;
  }
  if (members.data === undefined) {
    return null;
  }
  const rows = members.data.pages.flatMap((page) => page.data);
  const text = paged(rows.length, members.hasNextPage, "member", "members");
  if (!canInvite || invites.data === undefined) {
    return <StatusLine>{text}</StatusLine>;
  }
  const pending = invites.data.pages.flatMap((page) => page.data).length;
  return (
    <StatusLine>{`${text} · ${paged(pending, invites.hasNextPage, "pending invite", "pending invites")}`}</StatusLine>
  );
}

function ConnectionsStatus({ org }: { org: string }) {
  const query = $api.useQuery("get", "/api/v1/orgs/{org}/llm-connections", connectionsInit(org));
  if (query.isPending) {
    return <Skeleton className="h-4 w-32" />;
  }
  if (query.data === undefined) {
    return null;
  }
  const failing = query.data.data.filter((connection) => connection.status === "failing").length;
  return (
    <StatusLine>
      {plural(query.data.data.length, "connection", "connections")}
      {failing > 0 ? `, ${failing} failing` : ""}
    </StatusLine>
  );
}

function AssistantStatus({ org }: { org: string }) {
  const setting = $api.useQuery("get", "/api/v1/orgs/{org}/assistant", orgAssistantInit(org));
  const connections = $api.useQuery(
    "get",
    "/api/v1/orgs/{org}/llm-connections",
    connectionsInit(org),
  );
  if (setting.isPending || connections.isPending) {
    return <Skeleton className="h-4 w-40" />;
  }
  if (setting.data === undefined || connections.data === undefined) {
    return null;
  }
  const connectionId = setting.data.connection_id;
  const connection = connections.data.data.find((candidate) => candidate.id === connectionId);
  if (!setting.data.enabled || connection === undefined) {
    return <StatusLine>Off</StatusLine>;
  }
  return (
    <StatusLine>
      {["On", connection.name, setting.data.model].filter(Boolean).join(" · ")}
    </StatusLine>
  );
}

function OrgPricesStatus({ org }: { org: string }) {
  const query = $api.useQuery("get", "/api/v1/orgs/{org}/models", orgPricesInit(org));
  if (query.isPending) {
    return <Skeleton className="h-4 w-24" />;
  }
  if (query.data === undefined) {
    return null;
  }
  return <StatusLine>{plural(query.data.data.length, "own price", "own prices")}</StatusLine>;
}

function PlanStatus({ org }: { org: string }) {
  const { query } = useOrgEntitlements(org);
  if (query.isPending) {
    return <Skeleton className="h-4 w-48" />;
  }
  if (query.data === undefined) {
    return null;
  }
  const { plan, usage, limits } = query.data;
  const percent = usagePercent(usage.units, limits.units_included, limits.units_cap);
  return (
    <StatusLine>
      {percent === undefined
        ? `Plan: ${plan.name}`
        : `Plan: ${plan.name} · ${Math.round(percent)}% of units used`}
    </StatusLine>
  );
}

function SsoStatus({ org }: { org: string }) {
  const query = useQuery(orgSsoQuery(org));
  if (query.isPending) {
    return <Skeleton className="h-4 w-32" />;
  }
  if (query.data === undefined) {
    return null;
  }
  const provider = query.data.provider;
  if (provider === undefined) {
    return <StatusLine>No provider set up</StatusLine>;
  }
  if (!provider.enabled) {
    return <StatusLine>Disabled</StatusLine>;
  }
  return <StatusLine>{provider.enforced ? "Enabled and enforced" : "Enabled"}</StatusLine>;
}

/** The live status line of a project settings card. Only mounted for cards the role can see. */
function projectStatus(to: string, ctx: { org: string; project: string }) {
  const { org, project } = ctx;
  switch (to) {
    case "/$org/$project/settings/general":
      return <ProjectRetentionStatus />;
    case "/$org/$project/settings/api-keys":
      return <ApiKeysStatus org={org} project={project} />;
    case "/$org/$project/settings/data":
      return <DeletionStatus org={org} project={project} />;
    case "/$org/$project/settings/members":
      return <ProjectMembersStatus org={org} project={project} />;
    case "/$org/$project/settings/score-definitions":
      return <ProjectScoresStatus org={org} project={project} />;
    case "/$org/$project/settings/models":
      return <ProjectPricesStatus org={org} project={project} />;
    case "/$org/$project/settings/channels":
      return <ChannelsStatus org={org} project={project} />;
    default:
      return null;
  }
}

/** The live status line of an org settings card. Only mounted for cards the role can see. */
function orgStatus(to: string, ctx: { org: string; role: OrgRole | undefined }) {
  const { org, role } = ctx;
  switch (to) {
    case "/$org/settings/general":
      return <OrgSlugStatus org={org} />;
    case "/$org/settings/plan":
      return <PlanStatus org={org} />;
    case "/$org/settings/members":
      return <OrgMembersStatus org={org} canInvite={atLeast(role, "admin")} />;
    case "/$org/settings/llm-connections":
      return <ConnectionsStatus org={org} />;
    case "/$org/settings/assistant":
      return <AssistantStatus org={org} />;
    case "/$org/settings/models":
      return <OrgPricesStatus org={org} />;
    case "/$org/settings/sso":
      return <SsoStatus org={org} />;
    default:
      return null;
  }
}

function HomeCard({
  icon: Icon,
  label,
  description,
  status,
}: {
  icon: LucideIcon;
  label: string;
  description: string;
  status: ReactNode;
}) {
  return (
    <>
      <div className="flex items-center gap-2">
        <Icon aria-hidden="true" className="size-4 text-muted-foreground" />
        <span className="text-ui font-medium">{label}</span>
      </div>
      <p className="text-body text-muted-foreground">{description}</p>
      <div className="mt-auto pt-1">{status}</div>
    </>
  );
}

/** The org account, read once for the cards and the role checks. */
function useOrgRole(org: string): OrgRole | undefined {
  const me = $api.useQuery("get", "/api/auth/me");
  return me.data?.orgs.find((candidate) => candidate.slug === org)?.role;
}

function OrgCards({ org, role }: { org: string; role: OrgRole | undefined }) {
  const items = visibleOrgNav(role).filter((item) => item.to !== "/$org/settings");
  return (
    <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
      {items.map((item) => (
        <Link key={item.to} to={item.to} params={{ org }} className={CARD}>
          <HomeCard
            icon={item.icon}
            label={item.label}
            description={item.description}
            status={orgStatus(item.to, { org, role })}
          />
        </Link>
      ))}
    </div>
  );
}

/** The overview of the organization's settings, on `/<org>/settings`. */
export function OrgSettingsHome({ org }: { org: string }) {
  const role = useOrgRole(org);
  return (
    <SettingsPage
      title="Organization settings"
      description="Settings that apply to every project in this organization."
    >
      <OrgCards org={org} role={role} />
    </SettingsPage>
  );
}

/**
 * The overview of the project's settings, on `/<org>/<project>/settings`, with the
 * organization's cards below. Rendered inside the project route.
 */
export function ProjectSettingsHome() {
  const { org, project, data } = useProject();
  const orgRole = useOrgRole(org);
  const items = visibleProjectNav(data.role).filter(
    (item) => item.to !== "/$org/$project/settings",
  );
  return (
    <SettingsPage
      title="Project settings"
      description="Settings for this project. Organization settings are listed below."
    >
      <section aria-labelledby="project-settings-cards" className="flex flex-col gap-3">
        <h2 id="project-settings-cards" className="text-ui font-medium">
          Project
        </h2>
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {items.map((item) => (
            <Link key={item.to} to={item.to} params={{ org, project }} className={CARD}>
              <HomeCard
                icon={item.icon}
                label={item.label}
                description={item.description}
                status={projectStatus(item.to, { org, project })}
              />
            </Link>
          ))}
        </div>
      </section>
      <section aria-labelledby="org-settings-cards" className="flex flex-col gap-3">
        <h2 id="org-settings-cards" className="text-ui font-medium">
          Organization
        </h2>
        <OrgCards org={org} role={orgRole} />
      </section>
    </SettingsPage>
  );
}
