import { useQuery } from "@tanstack/react-query";
import { createFileRoute, Link, notFound } from "@tanstack/react-router";
import { ChevronLeft, Pencil } from "lucide-react";
import { type ReactNode, useState } from "react";
import { problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import {
  issueEventsQuery,
  issueOccurrencesQuery,
  issueQuery,
  usePatchIssue,
} from "@/api/queries-issues";
import type { Schema } from "@/api/types";
import { BACK_LINK_CLASS, BackLink } from "@/components/back-link";
import { FormError } from "@/components/form/form-error";
import { IssueActions } from "@/components/issues/issue-actions";
import { IssueHistory } from "@/components/issues/issue-history";
import { IssueStateBadge } from "@/components/issues/issue-state-badge";
import { OccurrenceChart, occurrenceWindow } from "@/components/issues/occurrence-chart";
import { ITEM_TITLE_CLASS, PageToolbar, PageToolbarTitle } from "@/components/page-toolbar";
import { RelativeTime } from "@/components/relative-time";
import { useProject } from "@/components/shell/project-context";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { Textarea } from "@/components/ui/textarea";
import { canAnnotate, canManageAlerts, canManageDatasets, canManageQueues } from "@/lib/roles";

/** Most examples the page lists: the API returns at most this many (D-IC-16). */
const EXAMPLES_SHOWN = 20;
/** Longest description the API keeps. */
const DESCRIPTION_MAX = 2000;
/** Longest title the API keeps. */
const TITLE_MAX = 200;

export const Route = createFileRoute("/$org/$project/issues/$issueId")({
  loader: async ({ context, params }) => {
    try {
      await context.queryClient.ensureQueryData(
        issueQuery(params.org, params.project, params.issueId),
      );
    } catch (error) {
      if (problemOf(error)?.status === 404) {
        throw notFound();
      }
      throw error;
    }
  },
  component: IssuePage,
});

function IssuePage() {
  const { org, project, data } = useProject();
  const { issueId } = Route.useParams();
  const canEdit = canAnnotate(data.role);
  // The 14-day chart window is fixed when the page opens, as the list's 24-hour window is.
  const [period] = useState(() => occurrenceWindow(Date.now()));
  const [titleError, setTitleError] = useState<string | undefined>();
  const detail = useQuery(issueQuery(org, project, issueId));
  const occurrences = useQuery(issueOccurrencesQuery(org, project, issueId, period));
  const events = useQuery(issueEventsQuery(org, project, issueId));

  if (detail.isPending) {
    return null;
  }
  if (detail.isError) {
    throw detail.error;
  }
  if (occurrences.isError) {
    throw occurrences.error;
  }
  if (events.isError) {
    throw events.error;
  }
  const issue = detail.data;
  const merged = issue.state === "merged";
  const days = occurrences.data?.days ?? [];
  const total = days.reduce((sum, day) => sum + day.count, 0);
  const busiestUsers = days.reduce((most, day) => Math.max(most, day.users), 0);
  const examples = (occurrences.data?.examples ?? []).slice(0, EXAMPLES_SHOWN);

  return (
    <div className="flex h-full min-h-0 w-full flex-col">
      <PageToolbar>
        <PageToolbarTitle
          back={
            <BackLink
              fallback={
                <Link
                  to="/$org/$project/issues"
                  params={{ org, project }}
                  aria-label="Back to issues"
                  className={BACK_LINK_CLASS}
                >
                  <ChevronLeft aria-hidden="true" className="size-3.5" />
                  Issues
                </Link>
              }
            />
          }
          label="Issue"
        >
          <IssueTitle
            org={org}
            project={project}
            issue={issue}
            canEdit={canEdit && !merged}
            onError={setTitleError}
          />
        </PageToolbarTitle>
      </PageToolbar>
      <div className="min-h-0 flex-1 overflow-auto">
        <div className="flex flex-col gap-6 px-3 py-3">
          <FormError message={titleError} />
          <div className="flex flex-wrap items-center gap-2 text-body">
            <IssueStateBadge state={issue.state} />
            {issue.regressed ? <Badge variant="destructive">Regressed</Badge> : null}
            <span className="text-muted-foreground">{issue.source_name}</span>
          </div>
          {merged && issue.merged_into ? (
            <MergedBanner org={org} project={project} targetId={issue.merged_into} />
          ) : null}
          <IssueActions
            org={org}
            project={project}
            issue={issue}
            canEdit={canEdit}
            canCreateAlert={canManageAlerts(data.role)}
            canAddExamples={canManageDatasets(data.role)}
            canSendToQueue={canManageQueues(data.role)}
          />

          <IssueDescription
            org={org}
            project={project}
            issue={issue}
            canEdit={canEdit && !merged}
          />

          <section className="flex flex-col gap-3">
            <dl className="grid grid-cols-2 gap-4 sm:grid-cols-4">
              <Stat label="Occurrences, 14 days">{occurrences.data ? total : "—"}</Stat>
              <Stat label="Users, busiest day">{occurrences.data ? busiestUsers : "—"}</Stat>
              <Stat label="First seen">
                <RelativeTime value={issue.first_seen} />
              </Stat>
              <Stat label="Last seen">
                <RelativeTime value={issue.last_seen} />
              </Stat>
            </dl>
            {occurrences.data ? (
              <OccurrenceChart days={days} fromMs={period.fromMs} />
            ) : (
              <Skeleton className="h-16 w-full" />
            )}
          </section>

          <section className="flex flex-col gap-2">
            <h3 className="text-body font-medium">Examples</h3>
            {!occurrences.data ? (
              <Skeleton className="h-8 w-full" />
            ) : examples.length === 0 ? (
              <p className="text-body text-muted-foreground">No examples yet.</p>
            ) : (
              <ul className="flex flex-col divide-y rounded-md border">
                {examples.map((example) => (
                  <li
                    key={`${example.issue_id}-${example.trace_id}-${example.created_at}`}
                    className="flex min-w-0 flex-wrap items-baseline gap-x-3 px-3 py-2"
                  >
                    <Link
                      to="/$org/$project/traces/$traceId"
                      params={{ org, project, traceId: example.trace_id }}
                      search={example.span_id ? { span: example.span_id } : {}}
                      className="focus-ring min-w-0 truncate rounded-sm text-body hover:underline"
                    >
                      {example.reason ?? `Trace ${example.trace_id.slice(0, 8)}`}
                    </Link>
                    <span className="ml-auto text-caption text-muted-foreground">
                      <RelativeTime value={example.created_at} />
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </section>

          <section className="flex flex-col gap-2">
            <h3 className="text-body font-medium">History</h3>
            <IssueHistory org={org} project={project} events={events.data?.data ?? []} />
          </section>
        </div>
      </div>
    </div>
  );
}

function Stat({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-0.5">
      <dt className="text-caption text-muted-foreground">{label}</dt>
      <dd className="text-body font-medium tabular-nums">{children}</dd>
    </div>
  );
}

/**
 * The title, with an inline edit for members. Enter saves, Esc cancels. Its errors go to `onError`,
 * which the page shows in the body: the toolbar is one row high.
 */
function IssueTitle({
  org,
  project,
  issue,
  canEdit,
  onError,
}: {
  org: string;
  project: string;
  issue: Schema<"Issue">;
  canEdit: boolean;
  onError: (message: string | undefined) => void;
}) {
  const patch = usePatchIssue(org, project);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(issue.title);

  if (!editing) {
    return (
      <span className="flex min-w-0 items-center gap-2">
        <h2 className={ITEM_TITLE_CLASS}>{issue.title}</h2>
        {canEdit ? (
          <Button
            type="button"
            variant="ghost"
            size="icon-xs"
            aria-label="Edit title"
            onClick={() => {
              setDraft(issue.title);
              onError(undefined);
              setEditing(true);
            }}
          >
            <Pencil aria-hidden="true" />
          </Button>
        ) : null}
      </span>
    );
  }

  function cancel() {
    onError(undefined);
    setEditing(false);
  }

  function save() {
    const title = draft.trim();
    if (title === issue.title) {
      cancel();
      return;
    }
    if (title === "") {
      onError("The title cannot be empty.");
      return;
    }
    onError(undefined);
    patch.mutate(
      {
        params: { path: { project: projectRef(org, project), id: issue.id } },
        body: { title },
      },
      {
        onSuccess: () => {
          setEditing(false);
        },
        onError: (caught) => {
          onError(problemMessage(caught, "Could not rename the issue."));
        },
      },
    );
  }

  return (
    <div className="flex min-w-0 flex-1 flex-col gap-1">
      <Input
        aria-label="Title"
        value={draft}
        maxLength={TITLE_MAX}
        autoFocus
        className="h-7 text-ui"
        onChange={(event) => {
          setDraft(event.target.value);
        }}
        onKeyDown={(event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            save();
          } else if (event.key === "Escape") {
            event.preventDefault();
            cancel();
          }
        }}
      />
    </div>
  );
}

/** The description, with an inline edit for members. An empty description clears it. */
function IssueDescription({
  org,
  project,
  issue,
  canEdit,
}: {
  org: string;
  project: string;
  issue: Schema<"Issue">;
  canEdit: boolean;
}) {
  const patch = usePatchIssue(org, project);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");
  const [error, setError] = useState<string | undefined>();

  function save() {
    const text = draft.trim();
    setError(undefined);
    patch.mutate(
      {
        params: { path: { project: projectRef(org, project), id: issue.id } },
        body: { description: text === "" ? null : text },
      },
      {
        onSuccess: () => {
          setEditing(false);
        },
        onError: (caught) => {
          setError(problemMessage(caught, "Could not save the description."));
        },
      },
    );
  }

  return (
    <section className="flex flex-col gap-2">
      <div className="flex items-center justify-between gap-2">
        <h3 className="text-body font-medium">Description</h3>
        {canEdit && !editing ? (
          <Button
            type="button"
            variant="ghost"
            size="sm"
            onClick={() => {
              setDraft(issue.description ?? "");
              setError(undefined);
              setEditing(true);
            }}
          >
            Edit description
          </Button>
        ) : null}
      </div>
      {editing ? (
        <div className="flex flex-col gap-2">
          <Textarea
            aria-label="Description"
            rows={4}
            maxLength={DESCRIPTION_MAX}
            value={draft}
            onChange={(event) => {
              setDraft(event.target.value);
            }}
          />
          <FormError message={error} />
          <div className="flex gap-2">
            <Button type="button" disabled={patch.isPending} onClick={save}>
              Save
            </Button>
            <Button
              type="button"
              variant="outline"
              disabled={patch.isPending}
              onClick={() => {
                setError(undefined);
                setEditing(false);
              }}
            >
              Cancel
            </Button>
          </div>
        </div>
      ) : issue.description ? (
        <p className="whitespace-pre-wrap text-body">{issue.description}</p>
      ) : (
        <p className="text-body text-muted-foreground">No description.</p>
      )}
    </section>
  );
}

/** Says where a merged issue went, with a link to it. */
function MergedBanner({
  org,
  project,
  targetId,
}: {
  org: string;
  project: string;
  targetId: string;
}) {
  const target = useQuery(issueQuery(org, project, targetId));
  return (
    <p role="status" className="rounded-md border bg-muted px-3 py-2 text-body">
      Merged into{" "}
      <Link
        to="/$org/$project/issues/$issueId"
        params={{ org, project, issueId: targetId }}
        className="font-medium underline"
      >
        {target.data?.title ?? "another issue"}
      </Link>
      .
    </p>
  );
}
