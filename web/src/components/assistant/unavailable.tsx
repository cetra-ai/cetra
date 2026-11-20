import { Link } from "@tanstack/react-router";
import type { Schema } from "@/api/types";
import { $api } from "@/api/client";
import { useProject } from "@/components/shell/project-context";
import { canManageConnections } from "@/lib/roles";

type Reason = NonNullable<Schema<"AssistantStatus">["reason"]>;

const MESSAGES: Record<Reason, string> = {
  forbidden: "The assistant is for members, admins and owners of this project.",
  disabled: "The assistant is off in this organization.",
  no_connection: "The assistant's LLM connection was deleted.",
  restore_mode: "The assistant is paused while this server is in restore mode.",
};

/** Why the assistant cannot answer. Org admins get a link to turn it on or pick a connection. */
export function Unavailable({ reason }: { reason: Reason | undefined }) {
  const { org } = useProject();
  const me = $api.useQuery("get", "/api/auth/me");
  const orgRole = me.data?.orgs.find((candidate) => candidate.slug === org)?.role;
  const offerLink =
    (reason === "disabled" || reason === "no_connection") && canManageConnections(orgRole);
  return (
    <div className="flex flex-col gap-3 p-4 text-body">
      <p>{reason ? MESSAGES[reason] : "The assistant is not available."}</p>
      {offerLink ? (
        <Link
          to="/$org/settings/assistant"
          params={{ org }}
          className="w-fit rounded-sm font-medium text-primary underline-offset-4 hover:underline"
        >
          Turn it on
        </Link>
      ) : null}
    </div>
  );
}
