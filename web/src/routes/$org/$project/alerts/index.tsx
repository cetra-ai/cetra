import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";
import { cn } from "cn";
import type { LucideIcon } from "lucide-react";
import { BellRing, Mail, MessageSquare, Plus, TriangleAlert, Webhook } from "lucide-react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import {
  alertRuleQuery,
  alertRulesQuery,
  channelsQuery,
  projectAlertEventsInit,
} from "@/api/queries-alerts";
import type { Schema } from "@/api/types";
import { EventKindBadge } from "@/components/alerts/event-kind-badge";
import { RuleTrend } from "@/components/alerts/rule-trend";
import { StateBadge } from "@/components/alerts/state-badge";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { Forbidden } from "@/components/forbidden";
import { FormError } from "@/components/form/form-error";
import { UpgradeButton } from "@/components/plan/upgrade-button";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  ALERT_BUCKETS,
  type AlertBucket,
  alertBucket,
  channelKindLabel,
  comparatorSymbol,
  conditionSummary,
  formatAlertValue,
} from "@/lib/alerts";
import { formatDuration } from "@/lib/format";
import { canManageAlerts } from "@/lib/roles";
import { parseAlertsSearch } from "@/lib/search/alerts";

export const Route = createFileRoute("/$org/$project/alerts/")({
  validateSearch: (search) => parseAlertsSearch(search),
  component: AlertsPage,
});

const CANNOT_MANAGE = "Only project members and above can manage alerts";

const BUCKET_LABEL: Record<AlertBucket, string> = {
  firing: "Firing",
  no_data: "No data",
  ok: "OK",
  disabled: "Disabled",
};

const KIND_ICON: Record<Schema<"ChannelKind">, LucideIcon> = {
  slack: MessageSquare,
  webhook: Webhook,
  email: Mail,
};

/** Channels shown by name before the "+N" overflow. */
const CHANNELS_SHOWN = 2;

function AlertsPage() {
  const { org, project, data } = useProject();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const query = useQuery(alertRulesQuery(org, project));
  const channels = useQuery(channelsQuery(org, project));
  const upgradeUrl = useOrgEntitlements(org).upgradeUrl;
  const events = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/alert-events",
    projectAlertEventsInit(org, project),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );

  if (query.error) {
    if (problemOf(query.error)?.status === 403) {
      return <Forbidden description="You don't have access to this project's alert rules." />;
    }
    throw query.error;
  }
  if (channels.error) {
    throw channels.error;
  }
  if (events.error) {
    throw events.error;
  }

  const canManage = canManageAlerts(data.role);
  const rules = query.data?.data ?? [];
  const history = events.data?.pages.flatMap((page) => page.data) ?? [];
  const empty = !query.isPending && rules.length === 0;
  const counts = countBuckets(rules);
  const shown = sortByBucket(rules).filter(
    (rule) => search.state === undefined || alertBucket(rule) === search.state,
  );
  const newRule = canManage ? (
    <Button asChild>
      <Link to="/$org/$project/alerts/new" params={{ org, project }}>
        <Plus />
        New rule
      </Link>
    </Button>
  ) : (
    <DisabledReason reason={CANNOT_MANAGE}>
      <Button disabled>
        <Plus />
        New rule
      </Button>
    </DisabledReason>
  );

  return (
    <div className="mx-auto flex w-full max-w-6xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="flex flex-col gap-1">
          <h1 className="text-lg font-medium">Alerts</h1>
          <p className="text-sm text-muted-foreground">
            Rules are evaluated every minute, or every 5 minutes for windows of 1 hour or more.
          </p>
        </div>
        {empty ? null : newRule}
      </div>
      {query.isPending ? (
        <>
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
        </>
      ) : empty ? (
        <>
          <EmptyState
            icon={BellRing}
            title="No alert rules yet"
            description="Alert on error rate, cost, latency or a score."
          />
          <div className="flex justify-center">{newRule}</div>
        </>
      ) : (
        <>
          <StateStrip
            counts={counts}
            active={search.state}
            onSelect={(state) => {
              void navigate({
                to: "/$org/$project/alerts",
                params: { org, project },
                search: (previous) => ({ ...previous, state }),
              });
            }}
          />
          <Table aria-label="Alert rules">
            <TableHeader>
              <TableRow>
                <TableHead>Name</TableHead>
                <TableHead>State</TableHead>
                <TableHead>Condition</TableHead>
                <TableHead>Last value</TableHead>
                <TableHead>Trend</TableHead>
                <TableHead>Last evaluated</TableHead>
                <TableHead>Channels</TableHead>
                <TableHead>Enabled</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {shown.length === 0 ? (
                <TableRow>
                  <TableCell colSpan={8} className="text-center text-muted-foreground">
                    No rules in this state.
                  </TableCell>
                </TableRow>
              ) : (
                shown.map((rule) => {
                  const bucket = alertBucket(rule);
                  return (
                    <TableRow key={rule.id}>
                      <TableCell
                        className={cn(
                          "border-l-2",
                          bucket === "firing" ? "border-l-destructive" : "border-l-transparent",
                        )}
                      >
                        <Link
                          to="/$org/$project/alerts/$ruleId"
                          params={{ org, project, ruleId: rule.id }}
                          className="font-medium underline-offset-4 hover:underline"
                        >
                          {rule.name}
                        </Link>
                      </TableCell>
                      <TableCell>
                        <div className="flex items-center gap-2">
                          <div className="flex flex-col items-start gap-0.5">
                            <StateBadge state={rule.state} enabled={rule.enabled} />
                            {bucket === "firing" && rule.last_fired_at ? (
                              <span className="text-xs text-muted-foreground tabular-nums">
                                for {formatDuration(elapsedNs(rule.last_fired_at))}
                              </span>
                            ) : null}
                          </div>
                          {rule.last_error ? (
                            <span title={rule.last_error} role="img" aria-label="Evaluation failed">
                              <TriangleAlert
                                aria-hidden="true"
                                className="size-4 text-destructive"
                              />
                            </span>
                          ) : null}
                        </div>
                      </TableCell>
                      <TableCell>{conditionSummary(rule.condition)}</TableCell>
                      <TableCell className="tabular-nums">
                        {formatAlertValue(rule.condition, rule.last_value)}
                      </TableCell>
                      <TableCell>
                        <RuleTrend org={org} project={project} rule={rule} />
                      </TableCell>
                      <TableCell>
                        {rule.last_evaluated_at ? (
                          <TimeText value={rule.last_evaluated_at} style="datetime" />
                        ) : (
                          "Never"
                        )}
                      </TableCell>
                      <TableCell>
                        <ChannelList ids={rule.channel_ids} channels={channels.data?.data} />
                      </TableCell>
                      <TableCell>
                        <EnabledSwitch
                          org={org}
                          project={project}
                          rule={rule}
                          canManage={canManage}
                          upgradeUrl={upgradeUrl}
                        />
                      </TableCell>
                    </TableRow>
                  );
                })
              )}
            </TableBody>
          </Table>
          <RecentEvents
            org={org}
            project={project}
            rules={rules}
            history={history}
            pending={events.isPending}
            hasNextPage={events.hasNextPage}
            loadingMore={events.isFetchingNextPage}
            onLoadMore={() => {
              void events.fetchNextPage();
            }}
          />
        </>
      )}
    </div>
  );
}

function RecentEvents({
  org,
  project,
  rules,
  history,
  pending,
  hasNextPage,
  loadingMore,
  onLoadMore,
}: {
  org: string;
  project: string;
  rules: readonly Schema<"AlertRule">[];
  history: readonly Schema<"AlertEvent">[];
  pending: boolean;
  hasNextPage: boolean;
  loadingMore: boolean;
  onLoadMore: () => void;
}) {
  const ruleById = new Map(rules.map((rule) => [rule.id, rule]));
  return (
    <section className="flex flex-col gap-3">
      <h2 className="text-base font-medium">Recent events</h2>
      {pending ? (
        <>
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
        </>
      ) : history.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          No events yet. Events appear when a rule fires or resolves.
        </p>
      ) : (
        <>
          <Table aria-label="Recent alert events">
            <TableHeader>
              <TableRow>
                <TableHead>Time</TableHead>
                <TableHead>Event</TableHead>
                <TableHead>Rule</TableHead>
                <TableHead>Value</TableHead>
                <TableHead>Deliveries</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {history.map((event) => {
                const rule = ruleById.get(event.rule_id);
                const deliveries = deliverySummary(event.deliveries);
                return (
                  <TableRow key={event.id}>
                    <TableCell>
                      <TimeText value={event.created_at} style="datetime" />
                    </TableCell>
                    <TableCell>
                      <EventKindBadge kind={event.kind} />
                    </TableCell>
                    <TableCell>
                      {rule ? (
                        <Link
                          to="/$org/$project/alerts/$ruleId"
                          params={{ org, project, ruleId: rule.id }}
                          className="font-medium underline-offset-4 hover:underline"
                        >
                          {rule.name}
                        </Link>
                      ) : (
                        <span className="text-muted-foreground">Deleted rule</span>
                      )}
                    </TableCell>
                    <TableCell className="tabular-nums">{eventValue(event, rule)}</TableCell>
                    <TableCell>
                      <span
                        className={cn(deliveries.failed && "text-destructive")}
                        title={deliveries.failedTitle}
                      >
                        {deliveries.text}
                      </span>
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
          {hasNextPage ? (
            <div>
              <Button
                type="button"
                variant="outline"
                size="sm"
                disabled={loadingMore}
                onClick={onLoadMore}
              >
                Load more
              </Button>
            </div>
          ) : null}
        </>
      )}
    </section>
  );
}

/**
 * The value against the threshold, as the rule page writes it. The rule's own condition formats
 * the numbers when the event is of the rule's metric; otherwise, or for a deleted rule, 3 decimals.
 */
function eventValue(event: Schema<"AlertEvent">, rule: Schema<"AlertRule"> | undefined): string {
  if (event.value === undefined) {
    return "—";
  }
  const format = (value: number) =>
    rule !== undefined && rule.condition.metric === event.metric
      ? formatAlertValue(rule.condition, value)
      : value.toFixed(3);
  return `${format(event.value)} ${comparatorSymbol(event.comparator)} ${format(event.threshold)}`;
}

/** "2 of 2 sent", or "No channels". `failed` when a delivery failed (destructive). */
function deliverySummary(deliveries: readonly Schema<"AlertDelivery">[]): {
  text: string;
  failed: boolean;
  failedTitle: string | undefined;
} {
  if (deliveries.length === 0) {
    return { text: "No channels", failed: false, failedTitle: undefined };
  }
  const sent = deliveries.filter((delivery) => delivery.status === "sent").length;
  const failures = deliveries.filter((delivery) => delivery.status === "failed");
  return {
    text: `${sent} of ${deliveries.length} sent`,
    failed: failures.length > 0,
    failedTitle:
      failures.length > 0
        ? failures
            .map((delivery) => `${delivery.channel_name}: ${delivery.last_error ?? "failed"}`)
            .join("\n")
        : undefined,
  };
}

function countBuckets(rules: readonly Schema<"AlertRule">[]): Record<AlertBucket, number> {
  const counts: Record<AlertBucket, number> = { firing: 0, no_data: 0, ok: 0, disabled: 0 };
  for (const rule of rules) {
    counts[alertBucket(rule)] += 1;
  }
  return counts;
}

/** Firing, No data, OK, Disabled, then by name. */
function sortByBucket(rules: readonly Schema<"AlertRule">[]): Schema<"AlertRule">[] {
  const rank = (rule: Schema<"AlertRule">) => ALERT_BUCKETS.indexOf(alertBucket(rule));
  return [...rules].sort((a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name));
}

/** Nanoseconds from an instant to now. */
function elapsedNs(rfc3339: string): number {
  return (Date.now() - Date.parse(rfc3339)) * 1_000_000;
}

function StateStrip({
  counts,
  active,
  onSelect,
}: {
  counts: Record<AlertBucket, number>;
  active: AlertBucket | undefined;
  onSelect: (state: AlertBucket | undefined) => void;
}) {
  return (
    <section aria-label="Summary" className="flex flex-wrap items-center gap-2 text-sm">
      {ALERT_BUCKETS.map((bucket) => {
        const selected = active === bucket;
        return (
          <button
            key={bucket}
            type="button"
            aria-pressed={selected}
            onClick={() => onSelect(selected ? undefined : bucket)}
            className={cn(
              "inline-flex items-center gap-1.5 rounded-md border px-2.5 py-1 hover:bg-muted",
              selected && "border-foreground/30 bg-muted font-medium",
            )}
          >
            <span>{BUCKET_LABEL[bucket]}</span>{" "}
            <span className="tabular-nums">{counts[bucket]}</span>
          </button>
        );
      })}
    </section>
  );
}

/** Kind icon and name per channel, two shown, then "+N". Unknown ids read "Deleted channel". */
function ChannelList({
  ids,
  channels,
}: {
  ids: string[];
  channels: Schema<"NotificationChannel">[] | undefined;
}) {
  if (ids.length === 0) {
    return (
      <span
        className="text-muted-foreground"
        title="Not sent anywhere: this rule only shows here and notifies nobody."
      >
        None
      </span>
    );
  }
  if (channels === undefined) {
    return <span className="text-muted-foreground">—</span>;
  }
  const found = ids.map((id) => channels.find((channel) => channel.id === id));
  const shown = found.slice(0, CHANNELS_SHOWN);
  const rest = found.slice(CHANNELS_SHOWN);
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
      {shown.map((channel, index) =>
        channel ? (
          <span key={channel.id} className="inline-flex items-center gap-1.5">
            <KindIcon kind={channel.kind} />
            {channel.name}
          </span>
        ) : (
          <span key={`deleted-${ids[index]}`} className="text-muted-foreground">
            Deleted channel
          </span>
        ),
      )}
      {rest.length > 0 ? (
        <span
          className="text-muted-foreground"
          title={rest.map((channel) => channel?.name ?? "Deleted channel").join(", ")}
        >
          +{rest.length}
        </span>
      ) : null}
    </div>
  );
}

function KindIcon({ kind }: { kind: Schema<"ChannelKind"> }) {
  const Icon = KIND_ICON[kind];
  const label = channelKindLabel(kind);
  return (
    <span role="img" aria-label={label} title={label} className="inline-flex">
      <Icon aria-hidden="true" className="size-3.5 text-muted-foreground" />
    </span>
  );
}

/** The rule's enabled flag, saved as it is toggled (as on the rule page). Errors show under it. */
function EnabledSwitch({
  org,
  project,
  rule,
  canManage,
  upgradeUrl,
}: {
  org: string;
  project: string;
  rule: Schema<"AlertRule">;
  canManage: boolean;
  upgradeUrl: string | undefined;
}) {
  const queryClient = useQueryClient();
  const update = $api.useMutation("patch", "/api/v1/projects/{project}/alert-rules/{id}");
  const problem = problemOf(update.error);
  const planLimited = problem?.status === 403 && problem.code === "plan_limit";

  return (
    <div className="flex flex-col items-start gap-1">
      <DisabledReason reason={canManage ? undefined : CANNOT_MANAGE}>
        <Switch
          aria-label={`Enable ${rule.name}`}
          checked={rule.enabled}
          disabled={!canManage || update.isPending}
          onCheckedChange={(enabled) => {
            update.mutate(
              {
                params: { path: { project: projectRef(org, project), id: rule.id } },
                body: { enabled },
              },
              {
                onSuccess: () => {
                  void queryClient.invalidateQueries({
                    queryKey: alertRulesQuery(org, project).queryKey,
                  });
                  void queryClient.invalidateQueries({
                    queryKey: alertRuleQuery(org, project, rule.id).queryKey,
                  });
                },
              },
            );
          }}
        />
      </DisabledReason>
      {update.error ? (
        <>
          <FormError message={problemMessage(update.error, "Could not update the rule.")} />
          {planLimited ? (
            <p className="text-xs">Upgrade your plan or disable another rule.</p>
          ) : null}
          {planLimited ? <UpgradeButton url={upgradeUrl} /> : null}
        </>
      ) : null}
    </div>
  );
}
