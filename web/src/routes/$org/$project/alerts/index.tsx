import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";
import { cn } from "cn";
import type { LucideIcon } from "lucide-react";
import { BellRing, Info, Mail, MessageSquare, Plus, TriangleAlert, Webhook } from "lucide-react";
import { type KeyboardEvent, useLayoutEffect, useRef } from "react";
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
import { RulePanel } from "@/components/alerts/rule-panel";
import { RuleTrend } from "@/components/alerts/rule-trend";
import { StateBadge } from "@/components/alerts/state-badge";
import { useShortcut } from "@/components/command/use-shortcut";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { FilterChips } from "@/components/filter-chips";
import { Forbidden } from "@/components/forbidden";
import { FormError } from "@/components/form/form-error";
import { TraceLayoutControls } from "@/components/layout-switcher";
import { PageToolbar } from "@/components/page-toolbar";
import { UpgradeButton } from "@/components/plan/upgrade-button";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { Filmstrip } from "@/components/trace/filmstrip";
import { SplitPane } from "@/components/trace/split-pane";
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
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
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
import { getTraceMaximized, setTraceMaximized, useListLayout } from "@/lib/list-layout";
import { canManageAlerts } from "@/lib/roles";
import { type AlertsSearch, parseAlertsSearch } from "@/lib/search/alerts";

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

  const queryClient = useQueryClient();
  const layout = useListLayout();
  const ruleOpen = search.rule !== undefined;
  const bodyRef = useRef<HTMLDivElement>(null);
  const listScroll = useRef(0);
  const wasOpen = useRef(ruleOpen);

  useLayoutEffect(() => {
    const node = bodyRef.current;
    if (node && wasOpen.current !== ruleOpen) {
      node.scrollTop = listScroll.current;
      node.dispatchEvent(new Event("scroll"));
      wasOpen.current = ruleOpen;
    }
    return () => {
      if (node) {
        listScroll.current = node.scrollTop;
      }
    };
  }, [ruleOpen]);

  useShortcut("dismiss", () => {
    if (getTraceMaximized()) {
      setTraceMaximized(false);
      return;
    }
    if (!search.rule) {
      return;
    }
    void navigate({
      to: "/$org/$project/alerts",
      params: { org, project },
      search: (prev) => withoutRule(prev),
    });
  });

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
    <Button asChild size="sm">
      <Link to="/$org/$project/alerts/new" params={{ org, project }}>
        <Plus />
        New rule
      </Link>
    </Button>
  ) : (
    <DisabledReason reason={CANNOT_MANAGE}>
      <Button disabled size="sm">
        <Plus />
        New rule
      </Button>
    </DisabledReason>
  );

  const filmstripItems = shown.map((rule) => {
    const bucket = alertBucket(rule);
    return {
      id: rule.id,
      label: rule.name,
      detail: BUCKET_LABEL[bucket],
      status: filmstripStatus(bucket),
    };
  });
  const openRule = (id: string, replace = false) => {
    void navigate({
      to: "/$org/$project/alerts",
      params: { org, project },
      search: (prev) => withRule(prev, id),
      replace,
    });
  };
  const closeRule = () => {
    void navigate({
      to: "/$org/$project/alerts",
      params: { org, project },
      search: (prev) => withoutRule(prev),
    });
  };
  /** Enter opens the row; with a rule open, the arrows step to the neighbouring row. */
  const onRowKeyDown = (event: KeyboardEvent<HTMLTableRowElement>, id: string) => {
    if (event.target !== event.currentTarget) {
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      openRule(id);
      return;
    }
    if (search.rule === undefined || (event.key !== "ArrowDown" && event.key !== "ArrowUp")) {
      return;
    }
    const sibling =
      event.key === "ArrowDown"
        ? event.currentTarget.nextElementSibling
        : event.currentTarget.previousElementSibling;
    if (!(sibling instanceof HTMLElement)) {
      return;
    }
    const siblingId = sibling.dataset.ruleId;
    if (siblingId === undefined) {
      return;
    }
    event.preventDefault();
    sibling.focus();
    openRule(siblingId, true);
  };

  return (
    <TooltipProvider>
      <div className="relative h-full min-h-0 flex-1">
        <div className="absolute inset-0">
          <SplitPane
            name="alerts"
            mode={layout}
            open={ruleOpen}
            filmstrip={
              <Filmstrip
                items={filmstripItems}
                selectedId={search.rule}
                onSelect={(id) => {
                  openRule(id);
                }}
                onBack={closeRule}
              />
            }
            start={
              <div className="flex h-full min-h-0 w-full flex-col">
                <PageToolbar actions={empty ? null : newRule}>
                  {query.isPending || empty ? null : (
                    <FilterChips
                      label="Summary"
                      chips={ALERT_BUCKETS.map((bucket) => ({
                        value: bucket,
                        label: BUCKET_LABEL[bucket],
                        count: counts[bucket],
                      }))}
                      active={search.state}
                      onSelect={(state) => {
                        void navigate({
                          to: "/$org/$project/alerts",
                          params: { org, project },
                          search: (previous) => ({ ...previous, state }),
                        });
                      }}
                    />
                  )}
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon-xs"
                        aria-label="How often rules are evaluated"
                      >
                        <Info />
                      </Button>
                    </TooltipTrigger>
                    <TooltipContent>
                      Rules are evaluated every minute, or every 5 minutes for windows of 1 hour or
                      more.
                    </TooltipContent>
                  </Tooltip>
                </PageToolbar>
                <div ref={bodyRef} data-scroll-container className="min-h-0 flex-1 overflow-auto">
                  <div className="flex flex-col gap-6 px-3 py-3">
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
                                <TableCell
                                  colSpan={8}
                                  className="text-center text-muted-foreground"
                                >
                                  No rules in this state.
                                </TableCell>
                              </TableRow>
                            ) : (
                              shown.map((rule) => {
                                const bucket = alertBucket(rule);
                                return (
                                  <TableRow
                                    key={rule.id}
                                    data-rule-id={rule.id}
                                    tabIndex={0}
                                    aria-selected={search.rule === rule.id}
                                    className={cn(
                                      "cursor-pointer",
                                      search.rule === rule.id && "bg-muted",
                                    )}
                                    onClick={() => {
                                      openRule(rule.id);
                                    }}
                                    onKeyDown={(event) => {
                                      onRowKeyDown(event, rule.id);
                                    }}
                                  >
                                    <TableCell
                                      className={cn(
                                        "border-l-2",
                                        bucket === "firing"
                                          ? "border-l-destructive"
                                          : "border-l-transparent",
                                      )}
                                    >
                                      <Link
                                        from="/$org/$project/alerts/"
                                        to="/$org/$project/alerts"
                                        params={{ org, project }}
                                        search={(prev) => withRule(prev, rule.id)}
                                        className="font-medium underline-offset-4 hover:underline"
                                        onClick={stopRowClick}
                                      >
                                        {rule.name}
                                      </Link>
                                    </TableCell>
                                    <TableCell>
                                      <div className="flex items-center gap-2">
                                        <div className="flex flex-col items-start gap-0.5">
                                          <StateBadge state={rule.state} enabled={rule.enabled} />
                                          {bucket === "firing" && rule.last_fired_at ? (
                                            <span className="text-caption text-muted-foreground tabular-nums">
                                              for {formatDuration(elapsedNs(rule.last_fired_at))}
                                            </span>
                                          ) : null}
                                        </div>
                                        {rule.last_error ? (
                                          <span
                                            title={rule.last_error}
                                            role="img"
                                            aria-label="Evaluation failed"
                                          >
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
                                        <TimeText
                                          value={rule.last_evaluated_at}
                                          style="date-minute"
                                        />
                                      ) : (
                                        "Never"
                                      )}
                                    </TableCell>
                                    <TableCell onClick={stopRowClick}>
                                      <ChannelList
                                        ids={rule.channel_ids}
                                        channels={channels.data?.data}
                                      />
                                    </TableCell>
                                    <TableCell onClick={stopRowClick}>
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
                </div>
              </div>
            }
            end={
              search.rule ? (
                <RulePanel
                  ruleId={search.rule}
                  row={rules.find((rule) => rule.id === search.rule)}
                  layout={<TraceLayoutControls />}
                  onClose={closeRule}
                  onDeleted={() => {
                    void queryClient.invalidateQueries({
                      queryKey: alertRulesQuery(org, project).queryKey,
                    });
                    void events.refetch();
                    closeRule();
                  }}
                />
              ) : null
            }
          />
        </div>
      </div>
    </TooltipProvider>
  );
}

function withRule(search: AlertsSearch, id: string): AlertsSearch {
  return { ...search, rule: id };
}

function withoutRule(search: AlertsSearch): AlertsSearch {
  const { rule: _rule, ...rest } = search;
  return rest;
}

/** Clicks on the cell's own controls (links, switch, channels) do not open the row's rule. */
function stopRowClick(event: { stopPropagation: () => void }) {
  event.stopPropagation();
}

/** Firing is an error, OK is ok, the rest have no status (SpanStatus "unset"). */
function filmstripStatus(bucket: AlertBucket): Schema<"SpanStatus"> {
  if (bucket === "firing") {
    return "error";
  }
  return bucket === "ok" ? "ok" : "unset";
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
      <h2 className="text-ui font-medium">Recent events</h2>
      {pending ? (
        <>
          <Skeleton className="h-8 w-full" />
          <Skeleton className="h-8 w-full" />
        </>
      ) : history.length === 0 ? (
        <p className="text-body text-muted-foreground">
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
                      <TimeText value={event.created_at} style="date-minute" />
                    </TableCell>
                    <TableCell>
                      <EventKindBadge kind={event.kind} />
                    </TableCell>
                    <TableCell>
                      {rule ? (
                        <Link
                          from="/$org/$project/alerts/"
                          to="/$org/$project/alerts"
                          params={{ org, project }}
                          search={(prev) => withRule(prev, rule.id)}
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
            <p className="text-caption">Upgrade your plan or disable another rule.</p>
          ) : null}
          {planLimited ? <UpgradeButton url={upgradeUrl} /> : null}
        </>
      ) : null}
    </div>
  );
}
