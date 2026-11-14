import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { Loader2 } from "lucide-react";
import { type FormEvent, useEffect, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { evaluatorsInit } from "@/api/queries-evaluation";
import { evalJobQuery } from "@/api/queries-online-rules";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { ERROR_HINTS } from "@/components/evaluators/error-hints";
import {
  defaultMappingFor,
  type MappingRow,
  MappingTable,
  mappingObject,
} from "@/components/evaluators/mapping-table";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { canManageEvaluation } from "@/lib/roles";

type Trace = Schema<"TraceSummary">;
type Target = "trace" | "span";
type Evaluator = Schema<"Evaluator">;
type Job = Schema<"EvalJob">;

const KIND_LABELS: Record<Evaluator["kind"], string> = {
  llm_judge: "LLM judge",
  check: "Check",
  webhook: "Webhook",
};

/** The trace and the span the trace view has selected. */
export interface RunTarget {
  trace: Trace;
  /** The selected span, when the trace view has one. */
  spanId?: string;
  /** Whether the selected span has no parent. A root span can only be run as a whole trace. */
  selectedIsRoot: boolean;
}

export function RunEvaluatorButton({ trace, spanId, selectedIsRoot }: RunTarget) {
  const { data } = useProject();
  const [open, setOpen] = useState(false);
  const canRun = canManageEvaluation(data.role);

  return (
    <>
      <DisabledReason reason={canRun ? undefined : "Only members can run evaluators"}>
        <Button
          type="button"
          variant="outline"
          size="xs"
          disabled={!canRun}
          onClick={() => {
            setOpen(true);
          }}
        >
          Run evaluator
        </Button>
      </DisabledReason>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Run evaluator</DialogTitle>
            <DialogDescription>
              Queues one run at the highest priority. Its score is written when the run finishes.
            </DialogDescription>
          </DialogHeader>
          <RunEvaluatorForm trace={trace} spanId={spanId} selectedIsRoot={selectedIsRoot} />
        </DialogContent>
      </Dialog>
    </>
  );
}

function RunEvaluatorForm({ trace, spanId, selectedIsRoot }: RunTarget) {
  const { org, project } = useProject();
  const queryClient = useQueryClient();
  const ref = projectRef(org, project);
  const evaluators = $api.useQuery(
    "get",
    "/api/v1/projects/{project}/evaluators",
    evaluatorsInit(org, project),
  );
  const [evaluatorId, setEvaluatorId] = useState("");
  const [target, setTarget] = useState<Target>("trace");
  const [mapping, setMapping] = useState<MappingRow[]>([]);
  const [fieldMessages, setFieldMessages] = useState<Record<string, string>>({});
  const [formMessage, setFormMessage] = useState<string | undefined>();
  const [jobId, setJobId] = useState<string | undefined>();
  const run = $api.useMutation("post", "/api/v1/projects/{project}/evaluators/{id}/run", {
    gcTime: 0,
  });
  const job = useQuery({
    ...evalJobQuery(org, project, jobId ?? ""),
    enabled: jobId !== undefined,
    refetchInterval: (query) => {
      const status = query.state.data?.status;
      return status === "queued" || status === "running" ? 1000 : false;
    },
  });
  const status = job.data?.status;

  // A finished run adds a score; the Scores tab reads the scores again.
  useEffect(() => {
    if (status === "done") {
      void queryClient.invalidateQueries({
        queryKey: ["get", "/api/v1/projects/{project}/scores"],
      });
    }
  }, [status, queryClient]);

  if (evaluators.error) {
    throw evaluators.error;
  }
  const list = evaluators.data?.data ?? [];
  const picked = list.find((item) => item.id === evaluatorId);
  // A span run needs a selected span that is not a root; otherwise the whole trace is the target.
  const spanTargetable = spanId !== undefined && !selectedIsRoot;
  const shownTarget: Target = spanTargetable ? target : "trace";

  function pickEvaluator(id: string) {
    setEvaluatorId(id);
    const next = list.find((item) => item.id === id);
    if (next) {
      setMapping(defaultMappingFor(next, shownTarget));
    }
  }

  function pickTarget(next: Target) {
    setTarget(next);
    if (picked) {
      setMapping(defaultMappingFor(picked, next));
    }
  }

  function onRunError(caught: unknown) {
    const problem = problemOf(caught);
    setFieldMessages({});
    setFormMessage(undefined);
    if (problem?.status === 429) {
      setFormMessage("Too many runs. Wait a minute.");
      return;
    }
    if (problem?.status === 404) {
      setFormMessage("Evaluator not found.");
      return;
    }
    const fields = fieldErrors(caught);
    if (Object.keys(fields).length > 0) {
      setFieldMessages(fields);
      return;
    }
    setFormMessage(problemMessage(caught, "Could not start the run."));
  }

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    if (evaluatorId.length === 0) {
      setFormMessage("Choose an evaluator.");
      return;
    }
    const onSpan = shownTarget === "span" && spanId !== undefined;
    setFieldMessages({});
    setFormMessage(undefined);
    run.mutate(
      {
        params: { path: { project: ref, id: evaluatorId } },
        body: {
          trace_id: trace.trace_id,
          ...(onSpan ? { span_id: spanId } : {}),
          trace_start: trace.start_time,
          variable_mapping: mappingObject(mapping),
        },
      },
      {
        onSuccess: (queued) => {
          setJobId(queued.id);
        },
        onError: onRunError,
      },
    );
  }

  const otherMessages = Object.entries(fieldMessages)
    .filter(([field]) => field !== "variable_mapping" && !field.startsWith("variable_mapping."))
    .map(([, message]) => message);
  const message = formMessage ?? (otherMessages.length > 0 ? otherMessages.join(" ") : undefined);

  return (
    <form noValidate className="flex flex-col gap-4" onSubmit={onSubmit}>
      <div className="flex flex-col gap-2">
        <Label htmlFor="run-evaluator-select">Evaluator</Label>
        <Select value={evaluatorId} onValueChange={pickEvaluator}>
          <SelectTrigger id="run-evaluator-select" className="w-fit">
            <SelectValue placeholder="Choose an evaluator" />
          </SelectTrigger>
          <SelectContent>
            {list.map((item) => (
              <SelectItem key={item.id} value={item.id}>
                {`${item.name} (${KIND_LABELS[item.kind]})`}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      {spanTargetable ? (
        <div className="flex flex-col gap-2">
          <span className="text-sm leading-none font-medium">Target</span>
          <RadioGroup
            aria-label="Target"
            value={shownTarget}
            onValueChange={(next) => {
              pickTarget(next === "span" ? "span" : "trace");
            }}
            className="flex-row gap-4"
          >
            <div className="flex items-center gap-2">
              <RadioGroupItem value="trace" id="run-target-trace" />
              <Label htmlFor="run-target-trace">Whole trace</Label>
            </div>
            <div className="flex items-center gap-2">
              <RadioGroupItem value="span" id="run-target-span" />
              <Label htmlFor="run-target-span">Selected span</Label>
            </div>
          </RadioGroup>
        </div>
      ) : null}

      <div className="flex flex-col gap-2">
        <MappingTable
          rows={mapping}
          onChange={setMapping}
          disabled={run.isPending}
          errorFor={(variable) => fieldMessages[`variable_mapping.${variable}`]}
        />
        {fieldMessages.variable_mapping ? (
          <p role="alert" className="text-xs text-destructive">
            {fieldMessages.variable_mapping}
          </p>
        ) : null}
      </div>

      <FormError message={message} />
      <RunOutcome org={org} project={project} job={job.data} jobId={jobId} />

      <div className="flex justify-end">
        <SubmitButton pending={run.isPending} disabled={evaluatorId.length === 0}>
          {run.isPending ? "Starting…" : "Run"}
        </SubmitButton>
      </div>
    </form>
  );
}

/** The run's state, read from the job the server answered with. */
function RunOutcome({
  org,
  project,
  job,
  jobId,
}: {
  org: string;
  project: string;
  job: Job | undefined;
  jobId: string | undefined;
}) {
  if (jobId === undefined) {
    return null;
  }
  if (job === undefined || job.status === "queued") {
    return <p className="text-sm">Queued</p>;
  }
  if (job.status === "running") {
    return (
      <p className="flex items-center gap-2 text-sm">
        <Loader2 className="size-4 animate-spin" aria-hidden="true" />
        Running
      </p>
    );
  }
  if (job.status === "done") {
    return (
      <div className="flex flex-col gap-1 text-sm">
        <p className="font-medium">Score written</p>
        {job.judge_trace_id ? (
          <Link
            to="/$org/$project/traces/$traceId"
            params={{ org, project, traceId: job.judge_trace_id }}
            className="w-fit text-xs underline underline-offset-4"
          >
            View judge trace
          </Link>
        ) : null}
      </div>
    );
  }
  if (job.status === "failed") {
    return (
      <div className="flex flex-col gap-1 text-sm">
        <p className="font-medium text-destructive">Run failed</p>
        {job.error ? <p>{job.error}</p> : null}
        {job.error_class ? (
          <p className="text-xs text-muted-foreground">{ERROR_HINTS[job.error_class]}</p>
        ) : null}
      </div>
    );
  }
  return <p className="text-sm">{`Skipped: ${job.error ?? "no reason given"}`}</p>;
}
