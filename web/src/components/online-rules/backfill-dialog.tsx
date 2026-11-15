import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage } from "@/api/problem";
import { backfillsKey, evalJobsKey, onlineRuleKey } from "@/api/queries-online-rules";
import type { Schema } from "@/api/types";
import { FormError } from "@/components/form/form-error";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { formatCost } from "@/lib/format";

type Rule = Schema<"OnlineRule">;
type WindowKey = "24h" | "7d" | "30d";

const WINDOWS: Record<WindowKey, { label: string; days: number }> = {
  "24h": { label: "Last 24 hours", days: 1 },
  "7d": { label: "Last 7 days", days: 7 },
  "30d": { label: "Last 30 days", days: 30 },
};
const DAY_MS = 86_400_000;
/** The server wants `to` at least 5 minutes ago; the extra minute covers clock skew. */
const TO_LAG_MS = 6 * 60_000;

/** The whole number the field holds when it is from 1 to 10 000. */
function maxTargetsOf(text: string): number | undefined {
  const trimmed = text.trim();
  if (!/^\d+$/.test(trimmed)) {
    return undefined;
  }
  const value = Number(trimmed);
  return value >= 1 && value <= 10_000 ? value : undefined;
}

/** The fraction the percent field holds when it is above 0 and at most 100. */
function sampleRateOf(percent: string): number | undefined {
  const value = Number(percent);
  if (percent.trim().length === 0 || !Number.isFinite(value) || value <= 0 || value > 100) {
    return undefined;
  }
  return Number((value / 100).toPrecision(12));
}

function percentOf(rate: number): string {
  return String(Number((rate * 100).toFixed(4)));
}

/** The window ends 6 minutes before now and starts `days` before that. */
function requestBody(windowKey: WindowKey, maxTargets: number, sampleRate: number) {
  const to = new Date(Date.now() - TO_LAG_MS);
  const from = new Date(to.getTime() - WINDOWS[windowKey].days * DAY_MS);
  return {
    from: from.toISOString(),
    to: to.toISOString(),
    max_targets: maxTargets,
    sample_rate: sampleRate,
  };
}

/** The estimate's sentences: the matches, the sampled targets and a judge's cost. */
function summaryOf(
  estimated: Schema<"BackfillEstimate">,
  maxTargets: number,
  noun: { one: string; many: string },
): string[] {
  const { matched, capped, sampled } = estimated;
  const lines = [
    capped
      ? `More than ${maxTargets.toLocaleString("en-US")} ${noun.many} match; the newest ${maxTargets.toLocaleString("en-US")} are used.`
      : `${matched.toLocaleString("en-US")} ${matched === 1 ? `${noun.one} matches` : `${noun.many} match`}.`,
    `${sampled.toLocaleString("en-US")} would be evaluated.`,
  ];
  if (estimated.estimated_cost_usd !== undefined) {
    const input = (estimated.estimated_input_tokens ?? 0).toLocaleString("en-US");
    const output = (estimated.estimated_output_tokens ?? 0).toLocaleString("en-US");
    lines.push(
      `Estimated cost: ${formatCost(estimated.estimated_cost_usd)} (${input} input and ${output} output tokens at most).`,
    );
  } else if (estimated.estimated_input_tokens !== undefined) {
    lines.push("The judge model has no price, so the cost cannot be estimated.");
  }
  return lines;
}

/** "Run on existing data": estimates a backfill of the rule, then starts it. */
export function BackfillDialog({
  open,
  onOpenChange,
  rule,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  rule: Rule;
}) {
  // Esc and the outside click must not close the dialog while the start request runs.
  const [starting, setStarting] = useState(false);
  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!starting) {
          onOpenChange(next);
        }
      }}
    >
      <DialogContent>
        <BackfillBody
          rule={rule}
          onStarting={setStarting}
          onStarted={() => {
            onOpenChange(false);
          }}
          onCancel={() => {
            onOpenChange(false);
          }}
        />
      </DialogContent>
    </Dialog>
  );
}

function BackfillBody({
  rule,
  onStarting,
  onStarted,
  onCancel,
}: {
  rule: Rule;
  onStarting: (starting: boolean) => void;
  onStarted: () => void;
  onCancel: () => void;
}) {
  const { org, project, ref } = useProject();
  const queryClient = useQueryClient();
  const estimator = $api.useMutation(
    "post",
    "/api/v1/projects/{project}/online-rules/{id}/backfills/estimate",
  );
  const starter = $api.useMutation(
    "post",
    "/api/v1/projects/{project}/online-rules/{id}/backfills",
  );
  const [windowKey, setWindowKey] = useState<WindowKey>("7d");
  const [maxText, setMaxText] = useState("1000");
  const [percentText, setPercentText] = useState(percentOf(rule.sample_rate));
  const [clientErrors, setClientErrors] = useState<Record<string, string>>({});

  const noun =
    rule.target === "span" ? { one: "span", many: "spans" } : { one: "trace", many: "traces" };
  // The estimate and the run share one body: the run sends the body the estimate was made with.
  const estimated = estimator.data;
  const sent = estimator.variables?.body;
  const serverErrors = fieldErrors(estimator.error);
  const hasServerErrors = Object.keys(serverErrors).length > 0;
  const windowError = serverErrors.from ?? serverErrors.to;
  const busy = estimator.isPending || starter.isPending;

  /** Any change clears the estimate: a run needs the estimate of the inputs on screen. */
  function change(apply: () => void) {
    apply();
    estimator.reset();
    setClientErrors({});
  }

  function estimate() {
    const maxTargets = maxTargetsOf(maxText);
    const sampleRate = sampleRateOf(percentText);
    const errors: Record<string, string> = {};
    if (maxTargets === undefined) {
      errors.max_targets = "Enter a whole number from 1 to 10,000.";
    }
    if (sampleRate === undefined) {
      errors.sample_rate = "Enter a percentage above 0 and at most 100.";
    }
    setClientErrors(errors);
    if (maxTargets === undefined || sampleRate === undefined) {
      return;
    }
    estimator.mutate({
      params: { path: { project: ref, id: rule.id } },
      body: requestBody(windowKey, maxTargets, sampleRate),
    });
  }

  function run() {
    if (sent === undefined) {
      return;
    }
    onStarting(true);
    starter.mutate(
      { params: { path: { project: ref, id: rule.id } }, body: sent },
      {
        onSuccess: () => {
          onStarting(false);
          void queryClient.invalidateQueries({ queryKey: backfillsKey(org, project, rule.id) });
          void queryClient.invalidateQueries({ queryKey: onlineRuleKey(org, project, rule.id) });
          void queryClient.invalidateQueries({ queryKey: evalJobsKey(org, project) });
          onStarted();
        },
        onError: () => {
          onStarting(false);
        },
      },
    );
  }

  const summary =
    estimated !== undefined && sent !== undefined
      ? summaryOf(estimated, sent.max_targets, noun)
      : [];

  const estimateError =
    estimator.isError && !hasServerErrors
      ? problemMessage(estimator.error, "The estimate could not be loaded.")
      : undefined;

  return (
    <>
      <DialogHeader>
        <DialogTitle>Run on existing data</DialogTitle>
        <DialogDescription>
          {`Evaluate ${noun.many} that were stored before the rule existed or while it was off. Targets the rule already scored are skipped.`}
        </DialogDescription>
      </DialogHeader>

      <div className="flex flex-col gap-4">
        <div className="flex flex-col gap-2">
          <Label htmlFor="backfill-window">Window</Label>
          <Select
            value={windowKey}
            onValueChange={(next) => {
              change(() => {
                setWindowKey(next as WindowKey);
              });
            }}
            disabled={busy}
          >
            <SelectTrigger
              id="backfill-window"
              className="w-fit"
              aria-invalid={windowError ? true : undefined}
            >
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {(Object.keys(WINDOWS) as WindowKey[]).map((key) => (
                <SelectItem key={key} value={key}>
                  {WINDOWS[key].label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          {windowError ? <p className="text-xs text-destructive">{windowError}</p> : null}
        </div>

        <div className="flex flex-col gap-2">
          <Label htmlFor="backfill-max">Max targets</Label>
          <Input
            id="backfill-max"
            name="backfill-max"
            type="number"
            min={1}
            max={10000}
            step={1}
            value={maxText}
            aria-invalid={clientErrors.max_targets || serverErrors.max_targets ? true : undefined}
            onChange={(event) => {
              change(() => {
                setMaxText(event.target.value);
              });
            }}
          />
          <p className="text-xs text-destructive">
            {clientErrors.max_targets ?? serverErrors.max_targets}
          </p>
        </div>

        <div className="flex flex-col gap-2">
          <Label htmlFor="backfill-sample">Sample rate (%)</Label>
          <Input
            id="backfill-sample"
            name="backfill-sample"
            type="number"
            min={0}
            max={100}
            step="any"
            value={percentText}
            aria-invalid={clientErrors.sample_rate || serverErrors.sample_rate ? true : undefined}
            onChange={(event) => {
              change(() => {
                setPercentText(event.target.value);
              });
            }}
          />
          <p className="text-xs text-destructive">
            {clientErrors.sample_rate ?? serverErrors.sample_rate}
          </p>
        </div>

        <div role="status" className="flex flex-col gap-1 text-sm">
          {summary.map((line) => (
            <p key={line}>{line}</p>
          ))}
        </div>

        <FormError message={estimateError} />
        <FormError
          message={
            starter.isError
              ? problemMessage(starter.error, "The backfill could not be started.")
              : undefined
          }
        />
      </div>

      <DialogFooter>
        <Button type="button" variant="outline" disabled={starter.isPending} onClick={onCancel}>
          Cancel
        </Button>
        <Button type="button" variant="outline" disabled={busy} onClick={estimate}>
          {estimator.isPending ? "Estimating…" : "Estimate"}
        </Button>
        <Button type="button" disabled={estimated === undefined || busy} onClick={run}>
          Run backfill
        </Button>
      </DialogFooter>
    </>
  );
}
