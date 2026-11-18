import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, Link, notFound, useNavigate } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import { useState } from "react";
import { $api } from "@/api/client";
import { type Problem, problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { evaluatorQuery, evaluatorsKey } from "@/api/queries-evaluation";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { DisabledReason } from "@/components/disabled-reason";
import { EvaluatorForm } from "@/components/evaluators/evaluator-form";
import { EvaluatorHistory } from "@/components/evaluators/evaluator-history";
import { EvaluatorOverview } from "@/components/evaluators/evaluator-overview";
import { RuleConflicts } from "@/components/online-rules/rule-conflicts";
import { useProject } from "@/components/shell/project-context";
import { TopBarCrumb } from "@/components/shell/top-bar-slot";
import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { canManageEvaluation } from "@/lib/roles";

type EvaluatorTab = "overview" | "config" | "history";

/** The page's search params. An unknown `tab` is dropped, and the page shows the overview. */
function parseEvaluatorSearch(search: Record<string, unknown>): { tab?: EvaluatorTab } {
  const tab =
    search.tab === "overview" || search.tab === "config" || search.tab === "history"
      ? search.tab
      : undefined;
  return tab ? { tab } : {};
}

export const Route = createFileRoute("/$org/$project/evaluators/$evaluatorId")({
  validateSearch: (search) => parseEvaluatorSearch(search),
  loader: async ({ context, params }) => {
    try {
      await context.queryClient.ensureQueryData(
        evaluatorQuery(params.org, params.project, params.evaluatorId),
      );
    } catch (error) {
      if (problemOf(error)?.status === 404) {
        throw notFound();
      }
      throw error;
    }
  },
  component: EvaluatorPage,
});

const KIND_LABELS = {
  llm_judge: "LLM judge",
  check: "Check",
  webhook: "Webhook",
} as const;

function EvaluatorPage() {
  const { org, project, data } = useProject();
  const { evaluatorId } = Route.useParams();
  const navigate = useNavigate();
  const search = Route.useSearch();
  const queryClient = useQueryClient();
  const detail = useQuery(evaluatorQuery(org, project, evaluatorId));
  const remove = $api.useMutation("delete", "/api/v1/projects/{project}/evaluators/{id}");
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [deleteFailure, setDeleteFailure] = useState<
    { message: string; problem: Problem | undefined } | undefined
  >();
  const canManage = canManageEvaluation(data.role);

  if (detail.isPending) {
    return null;
  }
  if (detail.isError) {
    throw detail.error;
  }
  const evaluator = detail.data;
  const tab = search.tab ?? "overview";

  function chooseTab(value: string) {
    const next = parseEvaluatorSearch({ tab: value }).tab;
    if (next === undefined) {
      return;
    }
    void navigate({
      to: "/$org/$project/evaluators/$evaluatorId",
      params: { org, project, evaluatorId },
      search: (prev) => ({ ...prev, tab: next }),
      // Keeps the history state, which carries the way back to where the evaluator was opened from.
      state: true,
    });
  }

  function onDelete() {
    setDeleteFailure(undefined);
    remove.mutate(
      { params: { path: { project: projectRef(org, project), id: evaluator.id } } },
      {
        onSuccess: () => {
          void queryClient.invalidateQueries({ queryKey: evaluatorsKey(org, project) });
          void navigate({ to: "/$org/$project/evaluators", params: { org, project } });
        },
        onError: (caught) => {
          setDeleteFailure({
            message: problemMessage(caught, "Could not delete the evaluator."),
            problem: problemOf(caught),
          });
        },
      },
    );
  }

  return (
    <div className="mx-auto flex w-full max-w-5xl flex-1 flex-col gap-6 overflow-auto px-4 py-6">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div className="flex flex-col gap-1">
          <div className="flex min-w-0 flex-wrap items-center gap-2">
            <BackLink
              fallback={
                <Link
                  to="/$org/$project/evaluators"
                  params={{ org, project }}
                  aria-label="Back to evaluators"
                  className={BACK_LINK_CLASS}
                >
                  <ChevronLeft aria-hidden="true" className="size-3.5" />
                  Evaluators
                </Link>
              }
            />
            <span aria-hidden="true" className="text-muted-foreground/60">
              /
            </span>
            <h2 className="text-title font-medium">{evaluator.name}</h2>
            <TopBarCrumb>
              <span className="max-w-64 truncate text-body font-medium">{evaluator.name}</span>
            </TopBarCrumb>
          </div>
          <p className="text-caption text-muted-foreground">
            {KIND_LABELS[evaluator.kind]} · version {evaluator.version} · score{" "}
            {evaluator.output_score.name} ({evaluator.output_score.data_type})
          </p>
        </div>
        <DisabledReason reason={canManage ? undefined : "Only members can edit evaluators"}>
          <Button
            type="button"
            variant="outline"
            disabled={!canManage}
            onClick={() => {
              setDeleteFailure(undefined);
              setConfirmOpen(true);
            }}
          >
            Delete
          </Button>
        </DisabledReason>
      </div>
      <Tabs value={tab} onValueChange={chooseTab} className="flex flex-col gap-4">
        <TabsList>
          <TabsTrigger value="overview">Overview</TabsTrigger>
          <TabsTrigger value="config">Configuration</TabsTrigger>
          <TabsTrigger value="history">History</TabsTrigger>
        </TabsList>
        <TabsContent value="overview">
          <EvaluatorOverview org={org} project={project} evaluator={evaluator} />
        </TabsContent>
        {/* Kept mounted while hidden so unsaved edits survive a tab switch. */}
        <TabsContent value="config" forceMount hidden={tab !== "config"}>
          {/* Keyed by version: a saved change remounts the form, which clears any secret it still holds. */}
          <EvaluatorForm
            key={`${evaluator.id}-${evaluator.version}`}
            org={org}
            project={project}
            evaluator={evaluator}
            onSaved={() => {
              void queryClient.invalidateQueries({
                queryKey: ["get", "/api/v1/projects/{project}/evaluators/{id}/versions"],
              });
            }}
          />
        </TabsContent>
        <TabsContent value="history">
          <EvaluatorHistory org={org} project={project} evaluatorId={evaluator.id} />
        </TabsContent>
      </Tabs>
      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={setConfirmOpen}
        title={`Delete ${evaluator.name}?`}
        description="Its test history is kept as evaluation traces."
        confirmLabel="Delete"
        destructive
        pending={remove.isPending}
        error={deleteFailure?.message}
        details={<RuleConflicts org={org} project={project} problem={deleteFailure?.problem} />}
        onConfirm={onDelete}
      />
    </div>
  );
}
