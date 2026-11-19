import { createFileRoute, redirect } from "@tanstack/react-router";

/** Alert notifications link here; the rule opens in the list's panel. */
export const Route = createFileRoute("/$org/$project/alerts/$ruleId/")({
  beforeLoad: ({ params }) => {
    throw redirect({
      to: "/$org/$project/alerts",
      params: { org: params.org, project: params.project },
      search: { rule: params.ruleId },
      replace: true,
    });
  },
});
