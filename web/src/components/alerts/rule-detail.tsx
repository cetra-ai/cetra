import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import {
  alertEventsInit,
  alertRuleQuery,
  alertRulesQuery,
  channelsQuery,
} from "@/api/queries-alerts";
import type { Schema } from "@/api/types";
import { EventKindBadge } from "@/components/alerts/event-kind-badge";
import { RulePreview } from "@/components/alerts/rule-preview";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { UpgradeButton } from "@/components/plan/upgrade-button";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
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
import { conditionSummary, formatAlertValue, formFromRule } from "@/lib/alerts";
import { canManageAlerts } from "@/lib/roles";

const CANNOT_MANAGE = "Only project members and above can manage alerts";

/**
 * A rule's content: enabled switch, edit and delete, condition, channels, the metric preview and
 * the history. Shared by the rule page and the rule panel. `onDeleted` runs after a successful
 * delete (the page navigates to the list, the panel tells its list).
 */
export function RuleDetail({
  rule,
  onDeleted,
}: {
  rule: Schema<"AlertRule">;
  onDeleted: () => void;
}) {
  const { org, project, data } = useProject();
  const queryClient = useQueryClient();
  const canEdit = canManageAlerts(data.role);
  const channels = useQuery(channelsQuery(org, project));
  const upgradeUrl = useOrgEntitlements(org).upgradeUrl;
  const events = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/alert-rules/{id}/events",
    alertEventsInit(org, project, rule.id),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );
  const update = $api.useMutation("patch", "/api/v1/projects/{project}/alert-rules/{id}");
  const remove = $api.useMutation("delete", "/api/v1/projects/{project}/alert-rules/{id}");
  // Never cleared on close, so the dialog does not blank while it animates out.
  const [deleteOpen, setDeleteOpen] = useState(false);

  const history = events.data?.pages.flatMap((page) => page.data) ?? [];
  const updateProblem = problemOf(update.error);
  const planLimited = updateProblem?.status === 403 && updateProblem.code === "plan_limit";

  return (
    <div className="flex flex-col gap-6">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="flex flex-col gap-2">
          <div className="flex items-center gap-2">
            <DisabledReason reason={canEdit ? undefined : CANNOT_MANAGE}>
              <Switch
                id="rule-enabled"
                checked={rule.enabled}
                disabled={!canEdit || update.isPending}
                onCheckedChange={(enabled) => {
                  update.mutate(
                    {
                      params: { path: { project: projectRef(org, project), id: rule.id } },
                      body: { enabled },
                    },
                    {
                      onSuccess: (next) => {
                        queryClient.setQueryData(
                          alertRuleQuery(org, project, rule.id).queryKey,
                          next,
                        );
                        void queryClient.invalidateQueries({
                          queryKey: alertRulesQuery(org, project).queryKey,
                        });
                      },
                    },
                  );
                }}
              />
            </DisabledReason>
            <Label htmlFor="rule-enabled">Enabled</Label>
          </div>
          <p className="text-xs text-muted-foreground">
            Disabling or changing the condition resets the state to OK.
          </p>
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
        <div className="flex items-center gap-2">
          <DisabledReason reason={canEdit ? undefined : CANNOT_MANAGE}>
            {canEdit ? (
              <Button asChild variant="outline" size="sm">
                <Link
                  to="/$org/$project/alerts/$ruleId/edit"
                  params={{ org, project, ruleId: rule.id }}
                >
                  Edit
                </Link>
              </Button>
            ) : (
              <Button type="button" variant="outline" size="sm" disabled>
                Edit
              </Button>
            )}
          </DisabledReason>
          <DisabledReason reason={canEdit ? undefined : CANNOT_MANAGE}>
            <Button
              type="button"
              variant="outline"
              size="sm"
              disabled={!canEdit}
              onClick={() => setDeleteOpen(true)}
            >
              Delete rule
            </Button>
          </DisabledReason>
        </div>
      </div>

      {rule.last_error ? (
        <p role="alert" className="text-sm text-destructive">
          The last evaluation failed: {rule.last_error}
        </p>
      ) : null}

      <dl className="grid grid-cols-[max-content_1fr] gap-x-6 gap-y-2 text-sm">
        <dt className="text-muted-foreground">Condition</dt>
        <dd>{conditionSummary(rule.condition)}</dd>
        <dt className="text-muted-foreground">Filter</dt>
        <dd>
          {rule.condition.filter ? (
            <span className="font-mono text-xs">{rule.condition.filter}</span>
          ) : (
            "All traces"
          )}
        </dd>
        {rule.condition.min_count !== undefined ? (
          <>
            <dt className="text-muted-foreground">Minimum samples</dt>
            <dd>{rule.condition.min_count}</dd>
          </>
        ) : null}
        <dt className="text-muted-foreground">Last value</dt>
        <dd className="tabular-nums">{formatAlertValue(rule.condition, rule.last_value)}</dd>
        <dt className="text-muted-foreground">Samples</dt>
        <dd className="tabular-nums">{rule.last_count ?? "—"}</dd>
        <dt className="text-muted-foreground">Last evaluated</dt>
        <dd>
          {rule.last_evaluated_at ? (
            <TimeText value={rule.last_evaluated_at} style="datetime" />
          ) : (
            "Never"
          )}
        </dd>
        <dt className="text-muted-foreground">Last fired</dt>
        <dd>
          {rule.last_fired_at ? <TimeText value={rule.last_fired_at} style="datetime" /> : "Never"}
        </dd>
        <dt className="text-muted-foreground">Channels</dt>
        <dd>
          {channels.error ? (
            <span className="text-destructive">
              {problemMessage(channels.error, "Could not load the channels.")}
            </span>
          ) : (
            channelNames(rule.channel_ids, channels.data?.data)
          )}
        </dd>
        <dt className="text-muted-foreground">Reminders</dt>
        <dd>
          {rule.notify_while_firing
            ? `Every ${Math.round(rule.cooldown_s / 60)} minutes while firing`
            : "Off"}
        </dd>
      </dl>

      <RulePreview org={org} project={project} values={formFromRule(rule)} />

      <div className="flex flex-col gap-3">
        <h2 className="text-base font-medium">History</h2>
        {events.error ? (
          <p role="alert" className="text-sm text-destructive">
            {problemMessage(events.error, "Could not load the history.")}
          </p>
        ) : events.isPending ? (
          <>
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
          </>
        ) : history.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            No events yet. Events appear when the rule fires or resolves.
          </p>
        ) : (
          <>
            <Table aria-label="Alert events">
              <TableHeader>
                <TableRow>
                  <TableHead>Time</TableHead>
                  <TableHead>Event</TableHead>
                  <TableHead>Value</TableHead>
                  <TableHead>Window</TableHead>
                  <TableHead>Notifications</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {history.map((event) => (
                  <TableRow key={event.id}>
                    <TableCell>
                      <TimeText value={event.created_at} style="datetime" />
                    </TableCell>
                    <TableCell>
                      <EventKindBadge kind={event.kind} />
                    </TableCell>
                    <TableCell className="tabular-nums">
                      {event.value === undefined ? (
                        "—"
                      ) : (
                        <>
                          <span>
                            {event.metric === rule.condition.metric
                              ? formatAlertValue(rule.condition, event.value)
                              : event.value.toFixed(3)}
                          </span>
                          {event.count !== undefined ? (
                            <span className="text-muted-foreground">
                              {" "}
                              · {event.count} {event.count === 1 ? "sample" : "samples"}
                            </span>
                          ) : null}
                        </>
                      )}
                    </TableCell>
                    <TableCell>
                      <TimeText value={event.window_from} style="time" />
                      {" – "}
                      <TimeText value={event.window_to} style="time" />
                    </TableCell>
                    <TableCell>
                      {event.deliveries.length === 0 ? (
                        "No channels"
                      ) : (
                        <ul className="flex flex-col gap-1">
                          {event.deliveries.map((delivery) => (
                            <li key={delivery.id} className="flex flex-wrap items-center gap-2">
                              <span>{delivery.channel_name}</span>
                              <DeliveryBadge status={delivery.status} />
                              {delivery.attempts > 1 ? (
                                <span className="text-muted-foreground">
                                  {delivery.attempts} attempts
                                </span>
                              ) : null}
                              {delivery.last_error ? (
                                <span className="text-xs text-muted-foreground">
                                  {delivery.last_error}
                                </span>
                              ) : null}
                            </li>
                          ))}
                        </ul>
                      )}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
            {events.hasNextPage ? (
              <div>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  disabled={events.isFetchingNextPage}
                  onClick={() => {
                    void events.fetchNextPage();
                  }}
                >
                  Load more
                </Button>
              </div>
            ) : null}
          </>
        )}
      </div>

      <ConfirmDialog
        open={deleteOpen}
        onOpenChange={setDeleteOpen}
        title="Delete alert rule?"
        description="Its history is deleted too."
        confirmLabel="Delete"
        destructive
        pending={remove.isPending}
        error={
          remove.error ? problemMessage(remove.error, "Could not delete the rule.") : undefined
        }
        onConfirm={() => {
          remove.mutate(
            { params: { path: { project: projectRef(org, project), id: rule.id } } },
            {
              onSuccess: () => {
                setDeleteOpen(false);
                remove.reset();
                void queryClient.invalidateQueries({
                  queryKey: alertRulesQuery(org, project).queryKey,
                });
                onDeleted();
              },
            },
          );
        }}
      />
    </div>
  );
}

/** "Team Slack, Pager webhook"; "—" while the channels load; "Deleted channel" for an unknown id. */
function channelNames(ids: string[], loaded: Schema<"NotificationChannel">[] | undefined): string {
  if (ids.length === 0) {
    return "None";
  }
  if (loaded === undefined) {
    return "—";
  }
  return ids
    .map((id) => loaded.find((channel) => channel.id === id)?.name ?? "Deleted channel")
    .join(", ");
}

function DeliveryBadge({ status }: { status: Schema<"DeliveryStatus"> }) {
  switch (status) {
    case "sent":
      return <Badge variant="secondary">Sent</Badge>;
    case "failed":
      return <Badge variant="destructive">Failed</Badge>;
    case "pending":
      return <Badge variant="outline">Pending</Badge>;
  }
}
