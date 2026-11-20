import { useQuery } from "@tanstack/react-query";
import { $api } from "@/api/client";
import { projectRef } from "@/api/project-ref";
import { issueQuery, issueSourcesQuery } from "@/api/queries-issues";
import type { Schema } from "@/api/types";
import { type Option, SelectField } from "@/components/alerts/select-field";
import type { IssueScope, RuleFormValues } from "@/lib/alerts";

const SCOPE_OPTIONS: Option<IssueScope>[] = [
  { value: "issue", label: "One issue" },
  { value: "source", label: "All issues of a source" },
];

const TITLE_LIMIT = 60;

/** A title cut to 60 characters and an ellipsis. */
export function cutTitle(title: string): string {
  const characters = [...title];
  if (characters.length <= TITLE_LIMIT) {
    return title;
  }
  return `${characters.slice(0, TITLE_LIMIT - 1).join("")}…`;
}

/**
 * The scope of an issue_occurrences rule (D-IC-20): one issue, or every issue of one source. Only
 * the select of the chosen scope loads its list.
 */
export function IssueScopeFields(props: {
  org: string;
  project: string;
  scope: IssueScope;
  issueId: string;
  sourceId: string;
  errors: Record<string, string>;
  disabled?: boolean;
  onChange: (patch: Partial<Pick<RuleFormValues, "issueScope" | "issueId" | "sourceId">>) => void;
}) {
  const { org, project, scope, issueId, sourceId, errors, disabled = false, onChange } = props;
  return (
    <>
      <SelectField
        id="rule-issue-scope"
        label="Scope"
        value={scope}
        options={SCOPE_OPTIONS}
        description={
          scope === "issue"
            ? "Counts occurrences in the window, including the issues merged into it."
            : "Counts occurrences of every issue of the source that is not ignored or merged."
        }
        disabled={disabled}
        onChange={(next) => {
          onChange({ issueScope: next });
        }}
      />
      {scope === "issue" ? (
        <IssueSelect
          org={org}
          project={project}
          issueId={issueId}
          disabled={disabled}
          error={errors["condition.issue_id"]}
          onChange={(next) => {
            onChange({ issueId: next });
          }}
        />
      ) : (
        <SourceSelect
          org={org}
          project={project}
          sourceId={sourceId}
          disabled={disabled}
          error={errors["condition.source_id"]}
          onChange={(next) => {
            onChange({ sourceId: next });
          }}
        />
      )}
    </>
  );
}

function IssueSelect({
  org,
  project,
  issueId,
  disabled,
  error,
  onChange,
}: {
  org: string;
  project: string;
  issueId: string;
  disabled: boolean;
  error?: string;
  onChange: (issueId: string) => void;
}) {
  const issues = useQuery(
    $api.queryOptions("get", "/api/v1/projects/{project}/issues", {
      params: {
        path: { project: projectRef(org, project) },
        query: { state: "all", limit: 100 },
      },
    }),
  );
  // A prefilled issue outside the first page is fetched, so the select always shows it.
  const listed = issues.data?.data ?? [];
  const listedPrefilled = listed.some((issue) => issue.id === issueId);
  const prefilled = useQuery({
    ...issueQuery(org, project, issueId),
    enabled: issueId !== "" && !listedPrefilled,
  });
  if (issues.error) {
    throw issues.error;
  }

  const options: Option<string>[] = listed.map((issue) => ({
    value: issue.id,
    label: issueLabel(issue),
  }));
  if (prefilled.data && !listedPrefilled) {
    options.unshift({ value: prefilled.data.id, label: issueLabel(prefilled.data) });
  }
  return (
    <>
      <SelectField
        id="rule-issue"
        label="Issue"
        value={issueId}
        options={options}
        disabled={disabled}
        onChange={onChange}
      />
      {error ? <p className="text-caption text-destructive">{error}</p> : null}
    </>
  );
}

function SourceSelect({
  org,
  project,
  sourceId,
  disabled,
  error,
  onChange,
}: {
  org: string;
  project: string;
  sourceId: string;
  disabled: boolean;
  error?: string;
  onChange: (sourceId: string) => void;
}) {
  const sources = useQuery(issueSourcesQuery(org, project));
  if (sources.error) {
    throw sources.error;
  }
  const options: Option<string>[] = (sources.data?.data ?? []).map((source) => ({
    value: source.id,
    label: source.name,
  }));
  return (
    <>
      <SelectField
        id="rule-source"
        label="Source"
        value={sourceId}
        options={options}
        disabled={disabled}
        onChange={onChange}
      />
      {error ? <p className="text-caption text-destructive">{error}</p> : null}
    </>
  );
}

function issueLabel(issue: Schema<"Issue">): string {
  return `${cutTitle(issue.title)} (${issue.source_name})`;
}
