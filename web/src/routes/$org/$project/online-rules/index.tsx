import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";
import { cn } from "cn";
import { ArrowDown, ArrowUp, Info, ListChecks, Loader2, Plus } from "lucide-react";
import { type KeyboardEvent, useLayoutEffect, useRef, useState } from "react";
import { $api } from "@/api/client";
import { useEvaluatorPages } from "@/api/queries-evaluation";
import {
  onlineRuleStatsKey,
  onlineRuleStatsQuery,
  onlineRulesInit,
  onlineRulesKey,
} from "@/api/queries-online-rules";
import type { Schema } from "@/api/types";
import { useShortcut } from "@/components/command/use-shortcut";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { TraceLayoutControls } from "@/components/layout-switcher";
import { EnabledSwitch } from "@/components/online-rules/enabled-switch";
import { EvaluatorCell } from "@/components/online-rules/evaluator-cell";
import { HourlyJobs } from "@/components/online-rules/hourly-jobs";
import { JobsBar } from "@/components/online-rules/jobs-bar";
import { RuleActions } from "@/components/online-rules/rule-actions";
import { ResumeButton } from "@/components/online-rules/rule-detail";
import { ruleStatusLabel } from "@/components/online-rules/rule-form";
import {
  borderTone,
  countBuckets,
  filterRules,
  jobTotal,
  sortRules,
  spendToday,
} from "@/components/online-rules/rule-list";
import { RulePanel } from "@/components/online-rules/rule-panel";
import {
  RULE_BUCKETS,
  type RuleBucket,
  RuleStatusPill,
  ruleBucket,
} from "@/components/online-rules/rule-status";
import { ruleSummary } from "@/components/online-rules/rule-summary";
import { ScoreSummary, scoreValue } from "@/components/online-rules/score-summary";
import { SpendBar } from "@/components/online-rules/spend-bar";
import { useProject } from "@/components/shell/project-context";
import { Filmstrip } from "@/components/trace/filmstrip";
import { SplitPane } from "@/components/trace/split-pane";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { formatCost } from "@/lib/format";
import { getTraceMaximized, setTraceMaximized, useListLayout } from "@/lib/list-layout";
import { useBackState } from "@/lib/list-return";
import { canManageEvaluation } from "@/lib/roles";
import {
  type OnlineRulesSearch,
  parseOnlineRulesSearch,
  type RuleSort,
} from "@/lib/search/online-rules";

export const Route = createFileRoute("/$org/$project/online-rules/")({
  validateSearch: (search) => parseOnlineRulesSearch(search),
  component: OnlineRulesPage,
});

const BUCKET_LABEL: Record<RuleBucket, string> = {
  active: "Active",
  paused: "Paused",
  disabled: "Disabled",
};

const BUCKET_DOT: Record<RuleBucket, string> = {
  active: "bg-status-ok",
  paused: "bg-status-warn",
  disabled: "bg-status-unset",
};

/** Columns that sort, and the direction a first click gives them. */
const SORT_FIRST_DIR: Record<RuleSort, "asc" | "desc"> = {
  name: "asc",
  status: "asc",
  spend: "desc",
  jobs: "desc",
  score: "desc",
};

function OnlineRulesPage() {
  const { org, project, data } = useProject();
  const canManage = canManageEvaluation(data.role);
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const layout = useListLayout();
  const queryClient = useQueryClient();
  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/online-rules",
    onlineRulesInit(org, project),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );
  const evaluators = useEvaluatorPages(org, project);
  const evaluatorItems = evaluators.data?.pages.flatMap((page) => page.data);
  const stats = useQuery(onlineRuleStatsQuery(org, project));
  const [text, setText] = useState(search.q ?? "");

  // The new rule's back link returns to this list, as it is now.
  const newBackState = useBackState("Online rules");
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
      to: "/$org/$project/online-rules",
      params: { org, project },
      search: (prev) => withoutRule(prev),
    });
  });

  if (query.error) {
    throw query.error;
  }

  const pages = query.data?.pages ?? [];
  const rows = pages.flatMap((page) => page.data);
  const activeRules = pages[0]?.org_enabled_rules;
  const evaluatorById = new Map((evaluatorItems ?? []).map((item) => [item.id, item]));
  const evaluatorName = (id: string) => evaluatorById.get(id)?.name;
  const statsByRule = new Map((stats.data?.data ?? []).map((entry) => [entry.rule_id, entry]));
  /** A rule's stats; none while they load or when the stats query failed (the cells then show nothing). */
  const statsOf = (ruleId: string) => (stats.isError ? undefined : statsByRule.get(ruleId));
  const empty = !query.isPending && rows.length === 0;
  const counts = countBuckets(rows);
  const shown = sortRules(
    filterRules(rows, { status: search.status, q: search.q, evaluatorName }),
    search.sort,
    search.dir,
    (rule) => scoreValue(statsOf(rule.id)?.scores),
  );
  const dailySpend = spendToday(rows);
  const jobs24 = rows.reduce((sum, rule) => sum + jobTotal(rule), 0);
  const failed24 = rows.reduce((sum, rule) => sum + rule.jobs_last_24h.failed, 0);
  const noEvaluators = evaluatorItems !== undefined && evaluatorItems.length === 0;

  const newRule = (
    <DisabledReason reason={canManage ? undefined : "Only members can create online rules"}>
      {canManage ? (
        <Button asChild>
          <Link to="/$org/$project/online-rules/new" params={{ org, project }} state={newBackState}>
            <Plus />
            New rule
          </Link>
        </Button>
      ) : (
        <Button disabled>
          <Plus />
          New rule
        </Button>
      )}
    </DisabledReason>
  );

  const filmstripItems = shown.map((rule) => ({
    id: rule.id,
    label: rule.name,
    detail: ruleStatusLabel(rule),
    status: filmstripStatus(ruleBucket(rule)),
  }));

  const openRule = (id: string, replace = false) => {
    void navigate({
      to: "/$org/$project/online-rules",
      params: { org, project },
      search: (prev) => withRule(prev, id),
      replace,
    });
  };
  const closeRule = () => {
    void navigate({
      to: "/$org/$project/online-rules",
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

  /** A column header: the first click sorts by it, the next flips the direction. */
  const sortBy = (key: RuleSort) => {
    void navigate({
      to: "/$org/$project/online-rules",
      params: { org, project },
      search: (prev) => {
        if (prev.sort === key) {
          return { ...prev, dir: (prev.dir ?? SORT_FIRST_DIR[key]) === "asc" ? "desc" : "asc" };
        }
        return { ...prev, sort: key, dir: SORT_FIRST_DIR[key] };
      },
    });
  };
  const sortHeader = (label: string, key: RuleSort) => {
    const active = search.sort === key;
    const dir = search.dir ?? SORT_FIRST_DIR[key];
    return (
      <TableHead aria-sort={active ? (dir === "asc" ? "ascending" : "descending") : undefined}>
        <Button
          type="button"
          variant="ghost"
          size="xs"
          className="-ml-2"
          onClick={() => {
            sortBy(key);
          }}
        >
          {label}
          {active ? dir === "asc" ? <ArrowUp /> : <ArrowDown /> : null}
        </Button>
      </TableHead>
    );
  };

  return (
    <TooltipProvider>
      <div className="relative h-full min-h-0 flex-1">
        <div className="absolute inset-0">
          <SplitPane
            name="online-rules"
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
                <div className="flex min-h-9 shrink-0 items-center gap-2 overflow-x-auto border-b px-3">
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <Button
                        type="button"
                        variant="ghost"
                        size="icon-xs"
                        aria-label="About online rules"
                      >
                        <Info />
                      </Button>
                    </TooltipTrigger>
                    <TooltipContent>
                      {`A rule runs an evaluator on new traces or spans that match its filter.${
                        activeRules === undefined
                          ? ""
                          : activeRules === 1
                            ? " 1 active rule in this organization."
                            : ` ${activeRules} active rules in this organization.`
                      }`}
                    </TooltipContent>
                  </Tooltip>
                  {query.isPending || empty ? null : (
                    <StatusStrip
                      counts={counts}
                      active={search.status}
                      onSelect={(status) => {
                        void navigate({
                          to: "/$org/$project/online-rules",
                          params: { org, project },
                          search: (previous) => ({ ...previous, status }),
                        });
                      }}
                    />
                  )}
                  {query.isPending || empty ? null : (
                    <div className="flex shrink-0 items-center gap-2 text-body tabular-nums text-muted-foreground">
                      <Separator orientation="vertical" className="h-4" />
                      {dailySpend === undefined ? null : (
                        <span>{`Spend today ${formatCost(dailySpend)}`}</span>
                      )}
                      <span>{`Jobs 24 h ${jobs24}`}</span>
                      {failed24 > 0 ? (
                        <span className="text-status-error">{`Failed ${failed24}`}</span>
                      ) : null}
                    </div>
                  )}
                  <div className="ml-auto flex shrink-0 items-center gap-2">
                    {query.isPending || empty ? null : (
                      <Input
                        aria-label="Search rules"
                        placeholder="Search rules"
                        value={text}
                        className="h-7 w-56 text-body"
                        onChange={(event) => {
                          const value = event.target.value;
                          setText(value);
                          void navigate({
                            to: "/$org/$project/online-rules",
                            params: { org, project },
                            search: (previous) => ({ ...previous, q: value || undefined }),
                            replace: true,
                          });
                        }}
                      />
                    )}
                    {empty ? null : newRule}
                  </div>
                </div>
                <div ref={bodyRef} data-scroll-container className="min-h-0 flex-1 overflow-auto">
                  <div className="flex flex-col gap-4 px-3 py-4">
                    {query.isPending ? (
                      <>
                        <Skeleton className="h-8 w-full" />
                        <Skeleton className="h-8 w-full" />
                        <Skeleton className="h-8 w-full" />
                      </>
                    ) : empty ? (
                      <>
                        <EmptyState
                          icon={ListChecks}
                          title="No online rules yet"
                          description="Score live traffic automatically: a rule runs an evaluator on new traces or spans that match its filter, on a sample, within a budget."
                        />
                        <div className="flex flex-col items-center gap-2">
                          {noEvaluators && canManage ? (
                            <>
                              <Button asChild>
                                <Link
                                  to="/$org/$project/evaluators/new"
                                  params={{ org, project }}
                                  state={newBackState}
                                >
                                  Create an evaluator
                                </Link>
                              </Button>
                              <p className="text-body text-muted-foreground">
                                A rule needs an evaluator first.
                              </p>
                            </>
                          ) : (
                            newRule
                          )}
                        </div>
                      </>
                    ) : (
                      <>
                        <Table aria-label="Online rules">
                          <TableHeader>
                            <TableRow>
                              {sortHeader("Name", "name")}
                              <TableHead>Evaluator</TableHead>
                              {sortHeader("Score", "score")}
                              {sortHeader("Status", "status")}
                              {sortHeader("Spend", "spend")}
                              {sortHeader("Last 24 h", "jobs")}
                              <TableHead>Enabled</TableHead>
                              <TableHead>
                                <span className="sr-only">Actions</span>
                              </TableHead>
                            </TableRow>
                          </TableHeader>
                          <TableBody>
                            {shown.length === 0 ? (
                              <TableRow>
                                <TableCell
                                  colSpan={8}
                                  className="text-center text-muted-foreground"
                                >
                                  No rules match.
                                </TableCell>
                              </TableRow>
                            ) : (
                              shown.map((rule) => (
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
                                  <TableCell className={cn("border-l-2", borderTone(rule))}>
                                    <Link
                                      from={Route.fullPath}
                                      to="/$org/$project/online-rules"
                                      params={{ org, project }}
                                      search={(prev) => withRule(prev, rule.id)}
                                      className="font-medium underline-offset-4 hover:underline"
                                      onClick={stopRowClick}
                                    >
                                      {rule.name}
                                    </Link>
                                    <div
                                      title={ruleSummary(rule)}
                                      className="max-w-[28rem] truncate font-mono text-caption text-muted-foreground"
                                    >
                                      {ruleSummary(rule)}
                                    </div>
                                  </TableCell>
                                  <TableCell>
                                    <EvaluatorCell
                                      evaluator={evaluatorById.get(rule.evaluator_id)}
                                      pending={evaluators.isPending}
                                      evaluatorId={rule.evaluator_id}
                                    />
                                  </TableCell>
                                  <TableCell>
                                    <ScoreSummary
                                      scores={statsOf(rule.id)?.scores}
                                      pending={stats.isPending}
                                    />
                                  </TableCell>
                                  <TableCell onClick={stopRowClick}>
                                    <div className="flex flex-col items-start gap-1">
                                      <RuleStatusPill rule={rule} />
                                      {canManage && rule.paused_reason !== undefined ? (
                                        <ResumeButton rule={rule} size="xs" />
                                      ) : null}
                                    </div>
                                  </TableCell>
                                  <TableCell>
                                    <SpendBar rule={rule} />
                                  </TableCell>
                                  <TableCell>
                                    <div className="flex flex-col items-start gap-1">
                                      <HourlyCell
                                        jobs={statsOf(rule.id)?.jobs_hourly}
                                        pending={stats.isPending}
                                      />
                                      <JobsBar counts={rule.jobs_last_24h} />
                                      {rule.dropped_last_hour > 0 ? (
                                        <span className="text-caption text-status-warn">
                                          {`${rule.dropped_last_hour} dropped this hour`}
                                        </span>
                                      ) : null}
                                    </div>
                                  </TableCell>
                                  <TableCell onClick={stopRowClick}>
                                    <EnabledSwitch rule={rule} canManage={canManage} />
                                  </TableCell>
                                  <TableCell onClick={stopRowClick}>
                                    <RuleActions
                                      rule={rule}
                                      canManage={canManage}
                                      onDeleted={() => {
                                        void queryClient.invalidateQueries({
                                          queryKey: onlineRulesKey(org, project),
                                        });
                                        void queryClient.invalidateQueries({
                                          queryKey: onlineRuleStatsKey(org, project),
                                        });
                                        if (search.rule === rule.id) {
                                          closeRule();
                                        }
                                      }}
                                    />
                                  </TableCell>
                                </TableRow>
                              ))
                            )}
                          </TableBody>
                        </Table>
                        {query.hasNextPage ? (
                          <Button
                            type="button"
                            variant="outline"
                            className="w-fit"
                            disabled={query.isFetchingNextPage}
                            onClick={() => {
                              void query.fetchNextPage();
                            }}
                          >
                            {query.isFetchingNextPage ? <Loader2 className="animate-spin" /> : null}
                            Load more
                          </Button>
                        ) : null}
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
                  row={rows.find((rule) => rule.id === search.rule)}
                  layout={<TraceLayoutControls />}
                  onClose={closeRule}
                />
              ) : null
            }
          />
        </div>
      </div>
    </TooltipProvider>
  );
}

function withRule(search: OnlineRulesSearch, id: string): OnlineRulesSearch {
  return { ...search, rule: id };
}

function withoutRule(search: OnlineRulesSearch): OnlineRulesSearch {
  const { rule: _rule, ...rest } = search;
  return rest;
}

/** Clicks on the cell's own controls (links, switch, buttons) do not open the row's rule. */
function stopRowClick(event: { stopPropagation: () => void }) {
  event.stopPropagation();
}

/** The hourly finished jobs above a row's totals: a skeleton while loading, nothing without stats. */
function HourlyCell({
  jobs,
  pending,
}: {
  jobs: Schema<"RuleJobsHour">[] | undefined;
  pending: boolean;
}) {
  if (pending) {
    return <Skeleton className="h-4 w-24" />;
  }
  return jobs === undefined ? null : <HourlyJobs jobs={jobs} />;
}

/** Active is ok, paused is an error, disabled has no status (SpanStatus "unset"). */
function filmstripStatus(bucket: RuleBucket): Schema<"SpanStatus"> {
  if (bucket === "active") {
    return "ok";
  }
  return bucket === "paused" ? "error" : "unset";
}

function StatusStrip({
  counts,
  active,
  onSelect,
}: {
  counts: Record<RuleBucket, number>;
  active: RuleBucket | undefined;
  onSelect: (status: RuleBucket | undefined) => void;
}) {
  return (
    <section aria-label="Status" className="flex shrink-0 items-center gap-2 text-body">
      {RULE_BUCKETS.map((bucket) => {
        const selected = active === bucket;
        return (
          <button
            key={bucket}
            type="button"
            aria-pressed={selected}
            onClick={() => onSelect(selected ? undefined : bucket)}
            className={cn(
              "inline-flex items-center gap-1.5 rounded-md border px-2 py-0.5 hover:bg-muted",
              selected && "border-foreground/30 bg-muted font-medium",
            )}
          >
            <span aria-hidden="true" className={cn("size-1.5 rounded-full", BUCKET_DOT[bucket])} />
            <span>{BUCKET_LABEL[bucket]}</span>{" "}
            <span className="tabular-nums">{counts[bucket]}</span>
          </button>
        );
      })}
    </section>
  );
}
