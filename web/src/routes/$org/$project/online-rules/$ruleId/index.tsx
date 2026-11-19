import { createFileRoute, redirect } from "@tanstack/react-router";

/** Online rule URLs link here; the rule opens in the list's panel. */
export const Route = createFileRoute("/$org/$project/online-rules/$ruleId/")({
  beforeLoad: ({ params }) => {
    throw redirect({
      to: "/$org/$project/online-rules",
      params: { org: params.org, project: params.project },
      search: { rule: params.ruleId },
      replace: true,
    });
  },
});
