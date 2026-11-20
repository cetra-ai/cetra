import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import type { ReactNode } from "react";
import { $api } from "@/api/client";
import { problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { evaluatorQuery } from "@/api/queries-evaluation";
import { connectionsInit } from "@/api/queries-settings";
import { DisabledReason } from "@/components/disabled-reason";
import { checkStateOf } from "@/components/evaluators/check-fields";
import { Markdown } from "@/components/payload/markdown";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { useIsMobile } from "@/hooks/use-mobile";
import { filterTarget, rangeSearchOf, sqlTabDraft } from "@/lib/assistant/apply";
import { handDraft } from "@/lib/assistant/handoff";
import {
  type DashboardDraft,
  type DraftKind,
  type EvaluatorDraft,
  type FilterDraft,
  parseDraft,
  type RuleDraft,
  type SqlDraft,
} from "@/lib/assistant/types";
import { canManageDashboards, canManageEvaluation } from "@/lib/roles";
import { MAX_TABS, readTabs } from "@/lib/sql/tabs";

const NO_EVALUATOR_PERMISSION = "Only members can edit evaluators";
const NO_RULE_PERMISSION = "Only members can edit online rules";
const NO_DASHBOARD_PERMISSION = "Only members, admins and owners edit dashboards.";
const WIDE_ONLY = "Dashboards are edited on wider screens.";

/** A fenced code block whose fence is longer than any backtick run in `body`. */
export function fenced(language: string, body: string): string {
  const longest = Math.max(2, ...(body.match(/`+/g) ?? []).map((run) => run.length));
  const fence = "`".repeat(longest + 1);
  return `${fence}${language}\n${body}\n${fence}`;
}

/** Why the SQL console cannot take one more tab, or `undefined` when it can. */
export function sqlTabsFullReason(ref: string): string | undefined {
  return readTabs(ref).tabs.length >= MAX_TABS
    ? `Close a SQL tab first: ${MAX_TABS} are open.`
    : undefined;
}

/** A dashboard widget's title, or its place when it has none. */
function titleOf(widget: unknown, index: number): string {
  const title =
    typeof widget === "object" && widget !== null
      ? (widget as { title?: unknown }).title
      : undefined;
  return typeof title === "string" && title.trim() !== "" ? title : `Widget ${index + 1}`;
}

function percentOf(rate: number): string {
  return `${Number((rate * 100).toFixed(4))}%`;
}

function CardFrame({
  title,
  action,
  children,
}: {
  title: string;
  action: ReactNode;
  children: ReactNode;
}) {
  return (
    <section
      aria-label={title}
      className="flex flex-col gap-2 rounded-md border bg-surface p-2.5 text-body"
    >
      <h3 className="font-medium">{title}</h3>
      {children}
      <div className="flex justify-end">{action}</div>
    </section>
  );
}

function ApplyButton({
  label,
  reason,
  onApply,
}: {
  label: string;
  reason: string | undefined;
  onApply: () => void;
}) {
  return (
    <DisabledReason reason={reason}>
      <Button type="button" size="sm" disabled={reason !== undefined} onClick={onApply}>
        {label}
      </Button>
    </DisabledReason>
  );
}

/**
 * A draft from the assistant, with what it holds and an Apply that opens its target prefilled.
 * Nothing is saved until the user saves there (D-AS-6, D-AS-28).
 */
export function DraftCard({ kind, draft }: { kind: DraftKind; draft: unknown }) {
  const parsed = parseDraft(kind, draft);
  if (parsed === null) {
    return (
      <p className="rounded-md border p-2.5 text-caption text-muted-foreground">
        This draft could not be read.
      </p>
    );
  }
  switch (parsed.kind) {
    case "filter":
      return <FilterCard draft={parsed.draft} />;
    case "sql":
      return <SqlCard draft={parsed.draft} />;
    case "dashboard":
      return <DashboardCard draft={parsed.draft} />;
    case "evaluator":
      return <EvaluatorCard draft={parsed.draft} />;
    case "online_rule":
      return <RuleCard draft={parsed.draft} />;
  }
}

function FilterCard({ draft }: { draft: FilterDraft }) {
  const { org, project } = useProject();
  const navigate = useNavigate();
  const target = filterTarget(draft);
  const searchLeftOut =
    draft.search !== undefined && (draft.view === "sessions" || draft.view === "users");
  const flags = [
    draft.include_eval ? "includes evaluation scores" : undefined,
    draft.include_experiments ? "includes experiments" : undefined,
  ].filter((flag): flag is string => flag !== undefined);
  const range =
    draft.range ??
    (draft.from !== undefined && draft.to !== undefined
      ? `${draft.from} to ${draft.to}`
      : "the list default");
  return (
    <CardFrame
      title="Filter"
      action={
        <ApplyButton
          label={`Open in ${draft.view}`}
          reason={undefined}
          onApply={() => {
            // The target's search is typed per list; its validateSearch drops any invalid key.
            void navigate({
              to: target.to,
              params: { org, project },
              search: target.search as never,
            });
          }}
        />
      }
    >
      <p>
        View: <span className="font-medium">{draft.view}</span>. Range: {range}.
      </p>
      {draft.filter !== undefined ? (
        <Markdown text={fenced("", draft.filter)} />
      ) : (
        <p className="text-muted-foreground">No filter.</p>
      )}
      {draft.search !== undefined && !searchLeftOut ? <p>Search: {draft.search}</p> : null}
      {searchLeftOut ? (
        <p className="text-caption text-muted-foreground">
          Search is not available on {draft.view}; it is left out.
        </p>
      ) : null}
      {flags.length > 0 ? (
        <p className="text-caption text-muted-foreground">{flags.join("; ")}</p>
      ) : null}
    </CardFrame>
  );
}

function SqlCard({ draft }: { draft: SqlDraft }) {
  const { org, project } = useProject();
  const navigate = useNavigate();
  const ref = projectRef(org, project);
  const params = Object.entries(sqlTabDraft(draft).params);
  return (
    <CardFrame
      title="SQL query"
      action={
        <ApplyButton
          label="Open in SQL console"
          reason={sqlTabsFullReason(ref)}
          onApply={() => {
            handDraft(ref, { kind: "sql", draft });
            // The console's validateSearch drops an invalid range, as for the list pages.
            void navigate({
              to: "/$org/$project/sql",
              params: { org, project },
              search: rangeSearchOf(draft) as never,
            });
          }}
        />
      }
    >
      <Markdown text={fenced("sql", draft.query)} />
      {params.length > 0 ? (
        <ul className="flex flex-col gap-0.5 text-caption">
          {params.map(([name, value]) => (
            <li key={name}>
              <span className="font-mono">{name}</span>: <span className="font-mono">{value}</span>
            </li>
          ))}
        </ul>
      ) : (
        <p className="text-caption text-muted-foreground">No parameters.</p>
      )}
      <p className="text-caption text-muted-foreground">
        {draft.columns.length > 0
          ? `Expected columns: ${draft.columns.map((column) => `${column.name} (${column.type})`).join(", ")}.`
          : "No expected columns."}
      </p>
    </CardFrame>
  );
}

function DashboardCard({ draft }: { draft: DashboardDraft }) {
  const { org, project, data } = useProject();
  const navigate = useNavigate();
  const narrow = useIsMobile();
  const ref = projectRef(org, project);
  const reason = !canManageDashboards(data.role)
    ? NO_DASHBOARD_PERMISSION
    : narrow
      ? WIDE_ONLY
      : undefined;
  return (
    <CardFrame
      title={draft.dashboard_id === undefined ? "New dashboard" : "Changes to this dashboard"}
      action={
        <ApplyButton
          label="Open in dashboard editor"
          reason={reason}
          onApply={() => {
            handDraft(ref, { kind: "dashboard", draft });
            if (draft.dashboard_id === undefined) {
              void navigate({
                to: "/$org/$project/dashboard",
                params: { org, project },
                search: (prev) => prev,
              });
            } else {
              void navigate({
                to: "/$org/$project/dashboard/$dashboardId",
                params: { org, project, dashboardId: draft.dashboard_id },
                search: (prev) => prev,
              });
            }
          }}
        />
      }
    >
      <p className="font-medium">{draft.name}</p>
      {draft.description ? <p className="text-muted-foreground">{draft.description}</p> : null}
      <ol className="list-decimal pl-5 text-caption">
        {draft.definition.widgets.map((widget, index) => (
          <li key={typeof widget?.id === "string" ? widget.id : index}>{titleOf(widget, index)}</li>
        ))}
      </ol>
    </CardFrame>
  );
}

function EvaluatorCard({ draft }: { draft: EvaluatorDraft }) {
  const { org, project, data } = useProject();
  const navigate = useNavigate();
  const ref = projectRef(org, project);
  const judge = draft.kind === "llm_judge";
  const connections = $api.useQuery(
    "get",
    "/api/v1/orgs/{org}/llm-connections",
    connectionsInit(org),
    { enabled: judge },
  );
  const connectionId =
    typeof draft.config.connection_id === "string" ? draft.config.connection_id : "";
  const connection = connections.data?.data.find((item) => item.id === connectionId);
  const connectionName = connection?.name ?? (connections.isPending ? "Loading…" : "Not found");
  const reason = canManageEvaluation(data.role) ? undefined : NO_EVALUATOR_PERMISSION;
  const score = draft.output_score;
  const check = judge ? undefined : checkStateOf(draft.config);
  return (
    <CardFrame
      title={`Evaluator: ${draft.name}`}
      action={
        <ApplyButton
          label="Open evaluator form"
          reason={reason}
          onApply={() => {
            handDraft(ref, { kind: "evaluator", draft });
            void navigate({ to: "/$org/$project/evaluators/new", params: { org, project } });
          }}
        />
      }
    >
      <p>{judge ? "LLM judge" : "Check"}</p>
      {judge ? (
        <>
          <p className="text-caption">
            Connection: {connectionName}. Model:{" "}
            {typeof draft.config.model === "string" && draft.config.model !== ""
              ? draft.config.model
              : "the connection's default"}
            .
          </p>
          <Markdown
            text={fenced(
              "text",
              typeof draft.config.prompt === "string" ? draft.config.prompt : "",
            )}
          />
        </>
      ) : check ? (
        <>
          <p className="text-caption">Check: {check.check}</p>
          <Markdown text={fenced("json", JSON.stringify(check.params, null, 2))} />
        </>
      ) : null}
      <p className="text-caption text-muted-foreground">
        Output score: {score.name} ({score.data_type})
        {score.categories !== undefined && score.categories.length > 0
          ? `; categories: ${score.categories.join(", ")}`
          : ""}
        .
      </p>
    </CardFrame>
  );
}

function RuleCard({ draft }: { draft: RuleDraft }) {
  const { org, project, data } = useProject();
  const navigate = useNavigate();
  const ref = projectRef(org, project);
  const evaluator = useQuery({ ...evaluatorQuery(org, project, draft.evaluator_id), retry: false });
  const unknown = problemOf(evaluator.error)?.status === 404;
  const reason = !canManageEvaluation(data.role)
    ? NO_RULE_PERMISSION
    : unknown
      ? "Unknown evaluator"
      : evaluator.isPending
        ? "Loading the evaluator…"
        : evaluator.isError
          ? "Could not read the evaluator."
          : undefined;
  const mapping = Object.entries(draft.variable_mapping);
  return (
    <CardFrame
      title={`Online rule: ${draft.name}`}
      action={
        <ApplyButton
          label="Open rule form"
          reason={reason}
          onApply={() => {
            handDraft(ref, { kind: "online_rule", draft });
            void navigate({ to: "/$org/$project/online-rules/new", params: { org, project } });
          }}
        />
      }
    >
      <p className="text-caption">
        Evaluator: {evaluator.data?.name ?? (unknown ? "Unknown evaluator" : "…")}
      </p>
      <p className="text-caption">
        Target: {draft.target === "span" ? "Spans" : "Traces"}. Sample rate:{" "}
        {percentOf(draft.sample_rate)}. {draft.enabled ? "Enabled." : "Disabled."}{" "}
        {draft.budget_usd !== undefined
          ? `Budget: $${draft.budget_usd} ${draft.budget_period ?? "day"}.`
          : "No budget."}
      </p>
      {draft.filter ? <Markdown text={fenced("", draft.filter)} /> : null}
      {mapping.length > 0 ? (
        <table className="w-full text-caption">
          <thead>
            <tr className="text-left text-muted-foreground">
              <th scope="col" className="pr-3 font-medium">
                Variable
              </th>
              <th scope="col" className="font-medium">
                Expression
              </th>
            </tr>
          </thead>
          <tbody>
            {mapping.map(([variable, expression]) => (
              <tr key={variable} className="align-top">
                <td className="pr-3 font-mono break-all">{variable}</td>
                <td className="font-mono break-all">{expression}</td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : (
        <p className="text-caption text-muted-foreground">No variables mapped.</p>
      )}
    </CardFrame>
  );
}
