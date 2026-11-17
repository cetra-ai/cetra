import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { X } from "lucide-react";
import type { ReactNode } from "react";
import { $api } from "@/api/client";
import { problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { datasetTrendsInit } from "@/api/queries-dataset-trends";
import { datasetQuery } from "@/api/queries-datasets";
import type { Schema } from "@/api/types";
import { ExperimentTrendChart } from "@/components/datasets/dataset-trend";
import { RunExperimentDialog } from "@/components/datasets/run-experiment-dialog";
import { StatusBadge } from "@/components/experiments/experiments-table";
import { RelativeTime } from "@/components/relative-time";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { TooltipProvider } from "@/components/ui/tooltip";
import { previewFields } from "@/lib/datasets";
import { useBackState } from "@/lib/list-return";

const PREVIEW_ROWS = 5;

/**
 * The dataset drawer beside the datasets list: the header with the name, then the dataset's facts,
 * its first items, experiments and versions. `row` is the dataset as the list has it, shown until
 * the fetch answers.
 */
export function DatasetPanel({
  datasetId,
  row,
  layout,
  onClose,
}: {
  datasetId: string;
  row?: Schema<"Dataset">;
  layout?: ReactNode;
  onClose: () => void;
}) {
  const { org, project } = useProject();
  const backState = useBackState("Datasets");
  const path = { project: projectRef(org, project), dataset: datasetId };
  const query = useQuery(datasetQuery(org, project, datasetId));
  // A dataset the list holds shows at once; once the fetch answers, its answer wins.
  const dataset = query.data ?? (query.isPending ? row : undefined);
  const missing = problemOf(query.error)?.status === 404;
  const items = $api.useQuery("get", "/api/v1/projects/{project}/datasets/{dataset}/items", {
    params: { path, query: { limit: PREVIEW_ROWS } },
  });
  const experiments = $api.useQuery("get", "/api/v1/projects/{project}/experiments", {
    params: {
      path: { project: projectRef(org, project) },
      query: { dataset: datasetId, limit: PREVIEW_ROWS },
    },
  });
  const versions = $api.useQuery("get", "/api/v1/projects/{project}/datasets/{dataset}/versions", {
    params: { path, query: { limit: PREVIEW_ROWS } },
  });
  const trends = $api.useQuery(
    "get",
    "/api/v1/projects/{project}/dataset-trends",
    datasetTrendsInit(org, project, [datasetId]),
  );

  return (
    <section aria-label="Dataset" className="flex h-full min-h-0 flex-col">
      <header className="flex shrink-0 items-center gap-2 border-b px-3 py-1.5 text-body">
        {dataset ? <span className="truncate text-ui font-semibold">{dataset.name}</span> : null}
        <div className="ml-auto flex shrink-0 items-center gap-1">
          {layout}
          <Button variant="ghost" size="xs" asChild>
            <Link
              to="/$org/$project/datasets/$datasetId"
              params={{ org, project, datasetId }}
              state={backState}
            >
              Open page
            </Link>
          </Button>
          <Button type="button" variant="ghost" size="icon-xs" aria-label="Close" onClick={onClose}>
            <X />
          </Button>
        </div>
      </header>
      <TooltipProvider>
        <div className="min-h-0 flex-1 overflow-auto p-3 text-body">
          {dataset ? (
            <div className="flex flex-col gap-3">
              {dataset.description ? (
                <p className="text-muted-foreground">{dataset.description}</p>
              ) : null}
              <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1">
                <Fact label="Items" value={String(dataset.item_count)} />
                <Fact label="Version" value={`v${dataset.current_version}`} />
                <Fact label="Experiments" value={String(dataset.experiment_count)} />
                <Fact
                  label="Created"
                  value={<TimeText value={dataset.created_at} style="datetime" />}
                />
                <Fact
                  label="Updated"
                  value={<TimeText value={dataset.updated_at} style="datetime" />}
                />
                <Fact label="Input schema" value={dataset.input_schema ? "set" : "none"} />
                <Fact label="Expected schema" value={dataset.expected_schema ? "set" : "none"} />
              </dl>
              <Section title="Items">
                {items.data ? (
                  items.data.data.length === 0 ? (
                    <p className="text-muted-foreground">No items in the latest version</p>
                  ) : (
                    <ul className="flex flex-col gap-1">
                      {items.data.data.map((item) => (
                        <li key={item.revision_id} className="truncate font-mono text-caption">
                          <Link
                            to="/$org/$project/datasets/$datasetId"
                            params={{ org, project, datasetId }}
                            search={{ item: item.id }}
                            state={backState}
                            className="hover:underline"
                          >
                            {previewFields(item.input)}
                          </Link>
                        </li>
                      ))}
                    </ul>
                  )
                ) : (
                  <Skeleton className="h-6 w-full" />
                )}
                {dataset.item_count > 0 ? (
                  <Link
                    to="/$org/$project/datasets/$datasetId"
                    params={{ org, project, datasetId }}
                    search={{ tab: "items" }}
                    state={backState}
                    className="self-start text-body text-muted-foreground hover:text-foreground"
                  >
                    View all {dataset.item_count} items
                  </Link>
                ) : null}
              </Section>
              <Section title="Experiments">
                {experiments.data ? (
                  experiments.data.data.length === 0 ? (
                    <p className="text-muted-foreground">No experiments yet</p>
                  ) : (
                    <ul className="flex flex-col gap-1">
                      {experiments.data.data.map((experiment) => (
                        <li key={experiment.id} className="flex min-w-0 flex-col gap-0.5">
                          <span className="flex min-w-0 items-center gap-1.5">
                            <Link
                              to="/$org/$project/experiments"
                              params={{ org, project }}
                              search={{ experiment: experiment.id }}
                              className="truncate underline-offset-4 hover:underline"
                            >
                              {experiment.name}
                            </Link>
                            <StatusBadge experiment={experiment} />
                          </span>
                          <span className="num flex items-center gap-1.5 text-caption text-muted-foreground">
                            <span>
                              {experiment.result_count}/{experiment.item_count} results
                            </span>
                            <span aria-hidden="true">·</span>
                            <RelativeTime value={experiment.created_at} />
                          </span>
                        </li>
                      ))}
                    </ul>
                  )
                ) : (
                  <Skeleton className="h-6 w-full" />
                )}
                <div className="flex flex-wrap items-center gap-2">
                  <RunExperimentDialog datasetId={datasetId} datasetName={dataset.name} />
                  {dataset.experiment_count > 0 ? (
                    <Link
                      to="/$org/$project/datasets/$datasetId"
                      params={{ org, project, datasetId }}
                      search={{ tab: "experiments" }}
                      state={backState}
                      className="text-body text-muted-foreground hover:text-foreground"
                    >
                      View all {dataset.experiment_count} experiments
                    </Link>
                  ) : null}
                </div>
              </Section>
              <Section title="Score trend">
                {trends.data ? (
                  <ExperimentTrendChart
                    experiments={trends.data.data[0]?.experiments ?? []}
                    unavailable={trends.data.scores_unavailable}
                  />
                ) : (
                  <Skeleton className="h-6 w-full" />
                )}
              </Section>
              <Section title="Recent versions">
                {versions.data ? (
                  versions.data.data.length === 0 ? (
                    <p className="text-muted-foreground">No versions</p>
                  ) : (
                    <ul className="flex flex-col gap-1">
                      {versions.data.data.map((version) => (
                        <li key={version.version} className="flex min-w-0 items-center gap-2">
                          <span className="num font-mono text-muted-foreground">
                            v{version.version}
                          </span>
                          <span className="min-w-0 truncate">{version.description}</span>
                          <span className="ml-auto shrink-0 text-muted-foreground">
                            <RelativeTime value={version.created_at} />
                          </span>
                        </li>
                      ))}
                    </ul>
                  )
                ) : (
                  <Skeleton className="h-6 w-full" />
                )}
              </Section>
            </div>
          ) : missing ? (
            <p className="text-muted-foreground">This dataset no longer exists.</p>
          ) : query.isError ? (
            <p className="text-destructive">
              {problemOf(query.error)?.title ?? "Could not load this dataset."}
            </p>
          ) : (
            <div className="flex flex-col gap-2">
              <Skeleton className="h-6 w-full" />
              <Skeleton className="h-6 w-full" />
              <Skeleton className="h-6 w-full" />
            </div>
          )}
        </div>
      </TooltipProvider>
    </section>
  );
}

function Fact({ label, value }: { label: string; value: ReactNode }) {
  return (
    <>
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="num min-w-0">{value}</dd>
    </>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-2 rounded-md border bg-surface p-3">
      <h3 className="text-caption font-medium tracking-wide text-muted-foreground uppercase">
        {title}
      </h3>
      {children}
    </section>
  );
}
