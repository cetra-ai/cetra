import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import type { ReactNode } from "react";
import { authConfigQuery } from "@/api/auth";
import { meQuery } from "@/api/queries";

/**
 * The read-only notice in the demo org (D187), shown by the org layout to a viewer of the demo org.
 * Its link leads to the visitor's own org, or to a way to create one. Renders nothing elsewhere.
 */
export function DemoBanner({ org }: { org: string }) {
  const { data: me } = useQuery(meQuery);
  const { data: config } = useQuery(authConfigQuery);
  const membership = me?.orgs.find((entry) => entry.slug === org);
  if (!me || !membership?.demo || membership.role !== "viewer") {
    return null;
  }
  const hasOwnOrg = me.orgs.some((entry) => !entry.demo);
  const createUrl = config?.org_create_url;
  let action: ReactNode = null;
  if (hasOwnOrg) {
    action = <Link to="/">Send your own traces</Link>;
  } else if (createUrl) {
    action = (
      <a href={createUrl} target="_blank" rel="noopener noreferrer">
        Send your own traces
      </a>
    );
  } else if (config?.org_create_allowed === true) {
    action = <Link to="/setup/workspace">Send your own traces</Link>;
  }
  return (
    <div
      role="status"
      aria-label="Demo"
      className="flex flex-wrap items-center gap-2 border-b bg-muted px-4 py-2 text-body"
    >
      <span>You&apos;re exploring the cetra demo, read-only.</span>
      {action === null ? null : (
        <span className="font-medium underline underline-offset-4">{action}</span>
      )}
    </div>
  );
}
