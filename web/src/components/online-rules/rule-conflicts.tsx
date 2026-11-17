import { Link } from "@tanstack/react-router";
import type { Problem } from "@/api/problem";

/**
 * The rules that block an evaluator change (D-I-19): a 409 whose errors name them as `rules.<id>`.
 * Renders nothing for any other problem.
 */
export function RuleConflicts({
  org,
  project,
  problem,
}: {
  org: string;
  project: string;
  problem: Problem | undefined;
}) {
  const conflicts = (problem?.status === 409 ? (problem.errors ?? []) : []).filter((item) =>
    item.field.startsWith("rules."),
  );
  if (conflicts.length === 0) {
    return null;
  }
  return (
    <ul className="flex flex-col gap-1 text-body">
      {conflicts.map((item) => (
        <li key={item.field}>
          <Link
            to="/$org/$project/online-rules/$ruleId"
            params={{ org, project, ruleId: item.field.slice("rules.".length) }}
            className="underline-offset-4 hover:underline"
          >
            {item.message}
          </Link>
        </li>
      ))}
    </ul>
  );
}
