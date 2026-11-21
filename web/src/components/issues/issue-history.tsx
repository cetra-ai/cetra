import { $api } from "@/api/client";
import { projectMembersInit } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { IdText } from "@/components/id-text";
import { RelativeTime } from "@/components/relative-time";

/** One history line in plain words. Values come from the issue's own events. */
export function eventText(event: Schema<"IssueEvent">): string {
  switch (event.kind) {
    case "created":
      return "Created";
    case "state_changed": {
      const from = event.from_value ?? "unknown";
      const to = event.to_value ?? "unknown";
      return `State changed from ${from} to ${to}`;
    }
    case "renamed":
      return event.to_value ? `Renamed to “${event.to_value}”` : "Renamed";
    case "merged":
      // The source of a merge records its target in `to_value`, the target records the source in `from_value`.
      return event.to_value
        ? "Merged into another issue"
        : "Another issue was merged into this one";
    case "regressed":
      return "Regressed: a resolved issue happened again";
    default:
      return event.kind;
  }
}

/**
 * The issue's history, newest first. The actor is a member of the project when the members page
 * holds them, else their id; the system (no actor) is "System". The members query is the one
 * the members page uses, so they share a cache entry.
 */
export function IssueHistory({
  org,
  project,
  events,
}: {
  org: string;
  project: string;
  events: readonly Schema<"IssueEvent">[];
}) {
  const members = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/members",
    projectMembersInit(org, project),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );
  const people = members.data?.pages.flatMap((page) => page.data) ?? [];
  if (events.length === 0) {
    return <p className="text-body text-muted-foreground">No history yet.</p>;
  }
  return (
    <ol className="flex flex-col divide-y rounded-md border">
      {events.map((event) => {
        const user = !event.actor_user_id
          ? undefined
          : people.find((member) => member.user.id === event.actor_user_id)?.user;
        return (
          <li key={event.id} className="flex flex-wrap items-baseline gap-x-3 gap-y-1 px-3 py-2">
            <span className="text-body">{eventText(event)}</span>
            <span className="text-caption text-muted-foreground">
              {!event.actor_user_id ? (
                "System"
              ) : user ? (
                user.name || user.email
              ) : (
                <IdText value={event.actor_user_id} kind="user" />
              )}
            </span>
            <span className="ml-auto text-caption text-muted-foreground">
              <RelativeTime value={event.at} />
            </span>
          </li>
        );
      })}
    </ol>
  );
}
