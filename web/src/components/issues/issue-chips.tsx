import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { cn } from "cn";
import { traceIssuesQuery } from "@/api/queries-issues";
import type { Schema } from "@/api/types";
import { useProject } from "@/components/shell/project-context";

/** Titles longer than this are cut to 39 characters and an ellipsis. */
const TITLE_LIMIT = 40;

/** Open needs attention (red), resolved is done (green), ignored and merged are muted (D-IC-2). */
const TONE: Record<Schema<"IssueState">, string> = {
  open: "bg-status-error/12 text-status-error",
  resolved: "bg-status-ok/12 text-status-ok",
  ignored: "bg-muted text-muted-foreground",
  merged: "bg-muted text-muted-foreground",
};

/** The issues of one trace, as chips linking to their issue pages. Renders nothing for none. */
export function IssueChips({ traceId }: { traceId: string }) {
  const { org, project } = useProject();
  const issues = useQuery(traceIssuesQuery(org, project, traceId));
  if (!issues.data || issues.data.length === 0) {
    return null;
  }
  return (
    <>
      {issues.data.map((issue) => (
        <Link
          key={issue.id}
          to="/$org/$project/issues/$issueId"
          params={{ org, project, issueId: issue.id }}
          title={issue.title}
          className={cn(
            "focus-ring inline-flex max-w-full rounded-sm px-1.5 text-caption",
            TONE[issue.state],
          )}
        >
          {truncateTitle(issue.title)}
        </Link>
      ))}
    </>
  );
}

function truncateTitle(title: string): string {
  const characters = [...title];
  if (characters.length <= TITLE_LIMIT) {
    return title;
  }
  return `${characters.slice(0, TITLE_LIMIT - 1).join("")}…`;
}
