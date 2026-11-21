import { useQuery } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";
import { Gauge, Loader2, Plus } from "lucide-react";
import { useEffect, useMemo } from "react";
import { $api } from "@/api/client";
import { evaluatorStatsQuery, useEvaluatorPages } from "@/api/queries-evaluation";
import { onlineRulesInit } from "@/api/queries-online-rules";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { detailOf, KIND_LABELS, KindIcon } from "@/components/evaluators/kind";
import {
  AvgCostCell,
  ErrorsCell,
  LatencyP50Cell,
  ResultCell,
  RunsCell,
  statsById,
} from "@/components/evaluators/stats-cells";
import { TemplateGallery } from "@/components/evaluators/template-gallery";
import { FilterChips } from "@/components/filter-chips";
import { ruleStatusLabel } from "@/components/online-rules/rule-form";
import { PageToolbar, PageToolbarSummary } from "@/components/page-toolbar";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { useBackState } from "@/lib/list-return";
import { canManageEvaluation } from "@/lib/roles";
import { parseEnum, parseString } from "@/lib/search/common";

type Rule = Schema<"OnlineRule">;
type KindFilter = Schema<"EvaluatorKind">;

const KINDS: readonly KindFilter[] = ["llm_judge", "check", "webhook"];
const QUERY_MAX = 128;

const KIND_FILTERS: { value: KindFilter; label: string }[] = [
  { value: "llm_judge", label: "LLM judge" },
  { value: "check", label: "Check" },
  { value: "webhook", label: "Webhook" },
];

type EvaluatorsSearch = { q?: string; kind?: KindFilter };

function parseEvaluatorsSearch(search: Record<string, unknown>): EvaluatorsSearch {
  const q = parseString(search.q, QUERY_MAX);
  const kind = parseEnum(search.kind, KINDS);
  return { ...(q ? { q } : {}), ...(kind ? { kind } : {}) };
}

export const Route = createFileRoute("/$org/$project/evaluators/")({
  validateSearch: (search) => parseEvaluatorsSearch(search),
  component: EvaluatorsPage,
});

/** The rules that use one evaluator, with their state and jobs over the last 24 hours. */
type Usage = { rules: number; active: number; paused: number; disabled: number; jobs: number };

function usageOf(rules: Rule[]): Usage {
  const usage: Usage = { rules: rules.length, active: 0, paused: 0, disabled: 0, jobs: 0 };
  for (const rule of rules) {
    const label = ruleStatusLabel(rule);
    if (label === "Active") {
      usage.active += 1;
    } else if (rule.enabled) {
      usage.paused += 1;
    } else {
      usage.disabled += 1;
    }
    usage.jobs += Object.values(rule.jobs_last_24h).reduce((sum, count) => sum + count, 0);
  }
  return usage;
}

/** A click on a link or a button inside a row belongs to that control, not to the row. */
function isControl(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest("a, button, input, [role=button]") !== null;
}

function UsedByCell({ rules, failed }: { rules?: Rule[]; failed: boolean }) {
  if (failed) {
    return <span className="text-muted-foreground">—</span>;
  }
  if (rules === undefined) {
    return <Skeleton className="h-4 w-24" />;
  }
  if (rules.length === 0) {
    return <span className="text-body text-muted-foreground">No rules</span>;
  }
  const usage = usageOf(rules);
  const states = [
    usage.active > 0 ? `${usage.active} active` : null,
    usage.paused > 0 ? `${usage.paused} paused` : null,
    usage.disabled > 0 ? `${usage.disabled} disabled` : null,
  ].filter((part): part is string => part !== null);
  return (
    <div className="flex flex-col">
      <span className="text-ui">{usage.rules === 1 ? "1 rule" : `${usage.rules} rules`}</span>
      <span className="text-body text-muted-foreground">
        {states.join(" · ")}
        {states.length > 0 ? " · " : ""}
        {`${usage.jobs} jobs / 24h`}
      </span>
    </div>
  );
}

function EvaluatorsPage() {
  const { org, project, data } = useProject();
  const canManage = canManageEvaluation(data.role);
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const backState = useBackState("Evaluators");
  const query = useEvaluatorPages(org, project);
  const rulesQuery = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/online-rules",
    onlineRulesInit(org, project),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );
  // The 7-day stats of every evaluator; a failed query leaves the stats cells showing a dash.
  const statsQuery = useQuery(evaluatorStatsQuery(org, project));
  const stats = statsById(statsQuery.data?.data);
  const { hasNextPage: rulesHaveMore, isFetchingNextPage: rulesFetching } = rulesQuery;
  const fetchMoreRules = rulesQuery.fetchNextPage;

  // The "Used by" column needs every rule, so the rule pages are read to the end.
  useEffect(() => {
    if (rulesHaveMore && !rulesFetching) {
      void fetchMoreRules();
    }
  }, [rulesHaveMore, rulesFetching, fetchMoreRules]);

  const rulesReady = rulesQuery.isSuccess && !rulesQuery.hasNextPage;
  const rows = query.data?.pages.flatMap((page) => page.data) ?? [];
  const q = search.q ?? "";
  const kind = search.kind;
  const filtering = q.trim().length > 0 || kind !== undefined;
  const visible = rows.filter(
    (evaluator) =>
      (kind === undefined || evaluator.kind === kind) &&
      evaluator.name.toLowerCase().includes(q.trim().toLowerCase()),
  );

  // The rules of each evaluator, once every rule page has loaded.
  const rulesByEvaluator = useMemo(() => {
    const map = new Map<string, Rule[]>();
    for (const rule of rulesQuery.data?.pages.flatMap((page) => page.data) ?? []) {
      map.set(rule.evaluator_id, [...(map.get(rule.evaluator_id) ?? []), rule]);
    }
    return map;
  }, [rulesQuery.data]);

  if (query.error) {
    throw query.error;
  }

  function updateSearch(change: EvaluatorsSearch) {
    void navigate({
      search: (prev) => ({ ...prev, ...change }),
      replace: true,
    });
  }

  function clearFilters() {
    void navigate({ search: {}, replace: true });
  }

  const newEvaluator = (
    <DisabledReason reason={canManage ? undefined : "Only members can create evaluators"}>
      {canManage ? (
        <Button asChild size="sm">
          <Link to="/$org/$project/evaluators/new" params={{ org, project }} state={backState}>
            <Plus />
            New evaluator
          </Link>
        </Button>
      ) : (
        <Button disabled size="sm">
          <Plus />
          New evaluator
        </Button>
      )}
    </DisabledReason>
  );

  return (
    <div className="flex h-full min-h-0 w-full flex-col">
      <PageToolbar actions={newEvaluator}>
        {query.isPending || rows.length === 0 ? null : (
          <>
            <Input
              type="search"
              aria-label="Search evaluators"
              placeholder="Search evaluators"
              className="h-7 w-56 text-body"
              value={q}
              maxLength={QUERY_MAX}
              onChange={(event) => updateSearch({ q: event.target.value || undefined })}
            />
            <FilterChips
              label="Kind"
              chips={KIND_FILTERS.map((filter) => ({
                ...filter,
                count: rows.filter((row) => row.kind === filter.value).length,
              }))}
              active={kind}
              onSelect={(value) => updateSearch({ kind: value })}
            />
            <PageToolbarSummary>
              {rows.length === 1 ? "1 evaluator" : `${rows.length} evaluators`}
            </PageToolbarSummary>
          </>
        )}
      </PageToolbar>
      <div className="min-h-0 flex-1 overflow-auto">
        <div className="flex flex-col gap-4 px-3 py-3">
          {canManage ? <TemplateGallery org={org} project={project} /> : null}
          {query.isPending ? (
            <>
              <Skeleton className="h-8 w-full" />
              <Skeleton className="h-8 w-full" />
              <Skeleton className="h-8 w-full" />
            </>
          ) : rows.length === 0 ? (
            <EmptyState
              icon={Gauge}
              title="No evaluators yet"
              description="Create one from a template to score traces with an LLM judge, a check or a webhook."
            />
          ) : visible.length === 0 ? (
            <div className="flex flex-col items-start gap-3 rounded-md border p-6">
              <p className="text-body">No evaluators match</p>
              {filtering ? (
                <Button type="button" variant="outline" size="sm" onClick={clearFilters}>
                  Clear filters
                </Button>
              ) : null}
            </div>
          ) : (
            <Table aria-label="Evaluators">
              <TableHeader>
                <TableRow>
                  <TableHead>Name</TableHead>
                  <TableHead>Kind</TableHead>
                  <TableHead>Output</TableHead>
                  <TableHead>Details</TableHead>
                  <TableHead>Used by</TableHead>
                  <TableHead>Runs (7d)</TableHead>
                  <TableHead>Errors</TableHead>
                  <TableHead>Avg cost</TableHead>
                  <TableHead>p50</TableHead>
                  <TableHead>Result (7d)</TableHead>
                  <TableHead>Version</TableHead>
                  <TableHead>Updated</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {visible.map((evaluator) => (
                  <TableRow
                    key={evaluator.id}
                    className="cursor-pointer"
                    onClick={(event) => {
                      // A click on a link or a button in the row belongs to that control.
                      if (
                        event.button !== 0 ||
                        event.metaKey ||
                        event.ctrlKey ||
                        event.shiftKey ||
                        isControl(event.target)
                      ) {
                        return;
                      }
                      void navigate({
                        to: "/$org/$project/evaluators/$evaluatorId",
                        params: { org, project, evaluatorId: evaluator.id },
                        state: backState,
                      });
                    }}
                  >
                    <TableCell>
                      <div className="flex items-center gap-2">
                        <KindIcon kind={evaluator.kind} />
                        <Link
                          to="/$org/$project/evaluators/$evaluatorId"
                          params={{ org, project, evaluatorId: evaluator.id }}
                          state={backState}
                          className="rounded-sm font-medium underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
                        >
                          {evaluator.name}
                        </Link>
                      </div>
                    </TableCell>
                    <TableCell>{KIND_LABELS[evaluator.kind]}</TableCell>
                    <TableCell>
                      <div className="flex flex-col">
                        <span>{evaluator.output_score.name}</span>
                        <span className="text-caption text-muted-foreground">
                          {evaluator.output_score.data_type}
                        </span>
                      </div>
                    </TableCell>
                    <TableCell>
                      <span className="font-mono text-body">{detailOf(evaluator)}</span>
                    </TableCell>
                    <TableCell>
                      <UsedByCell
                        rules={rulesReady ? (rulesByEvaluator.get(evaluator.id) ?? []) : undefined}
                        failed={rulesQuery.isError}
                      />
                    </TableCell>
                    <TableCell className="whitespace-nowrap">
                      <RunsCell stats={stats.get(evaluator.id)} pending={statsQuery.isPending} />
                    </TableCell>
                    <TableCell className="whitespace-nowrap">
                      <ErrorsCell stats={stats.get(evaluator.id)} pending={statsQuery.isPending} />
                    </TableCell>
                    <TableCell className="whitespace-nowrap">
                      <AvgCostCell stats={stats.get(evaluator.id)} pending={statsQuery.isPending} />
                    </TableCell>
                    <TableCell className="whitespace-nowrap">
                      <LatencyP50Cell
                        stats={stats.get(evaluator.id)}
                        pending={statsQuery.isPending}
                      />
                    </TableCell>
                    <TableCell className="whitespace-nowrap">
                      <ResultCell stats={stats.get(evaluator.id)} pending={statsQuery.isPending} />
                    </TableCell>
                    <TableCell>{evaluator.version}</TableCell>
                    <TableCell>
                      <TimeText value={evaluator.updated_at} style="datetime" />
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
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
        </div>
      </div>
    </div>
  );
}
