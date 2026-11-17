import { $api } from "@/api/client";
import { projectMembersInit } from "@/api/queries-settings";
import { IdText } from "@/components/id-text";
import { useProject } from "@/components/shell/project-context";

/**
 * The author of a version: the member's name (or email) when the project's first page of members
 * holds them, else the user id; "API key" for a key (`key:<id>`); a dash when there is no author.
 * The members query is the members page's, so the two share one cache entry.
 */
export function CreatedBy({ createdBy }: { createdBy: string | undefined }) {
  if (createdBy === undefined) {
    return "—";
  }
  if (createdBy.startsWith("key:")) {
    return "API key";
  }
  return <MemberName userId={createdBy} />;
}

function MemberName({ userId }: { userId: string }) {
  const { org, project } = useProject();
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
  const user = members.data?.pages
    .flatMap((page) => page.data)
    .find((member) => member.user.id === userId)?.user;
  if (!user) {
    return <IdText value={userId} kind="user" />;
  }
  return (
    <span className="truncate" title={user.email}>
      {user.name || user.email}
    </span>
  );
}
