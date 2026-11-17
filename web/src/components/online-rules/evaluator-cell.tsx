import { Link } from "@tanstack/react-router";
import type { Schema } from "@/api/types";
import { KIND_LABELS, KindIcon } from "@/components/evaluators/kind";
import { useProject } from "@/components/shell/project-context";
import { Skeleton } from "@/components/ui/skeleton";

/** The rule's evaluator: its name and kind, linked to the evaluator. Clicks do not reach the row. */
export function EvaluatorCell({
  evaluator,
  pending,
  evaluatorId,
}: {
  evaluator: Schema<"Evaluator"> | undefined;
  pending: boolean;
  evaluatorId: string;
}) {
  const { org, project } = useProject();
  if (pending) {
    return <Skeleton className="h-4 w-24" />;
  }
  if (evaluator === undefined) {
    return (
      <span className="text-muted-foreground" title={evaluatorId}>
        Unknown evaluator
      </span>
    );
  }
  return (
    <Link
      to="/$org/$project/evaluators/$evaluatorId"
      params={{ org, project, evaluatorId }}
      className="flex flex-col underline-offset-4 hover:underline"
      onClick={(event) => {
        event.stopPropagation();
      }}
    >
      <span className="flex items-center gap-1.5">
        <KindIcon kind={evaluator.kind} className="size-3.5" />
        {evaluator.name}
      </span>
      <span className="text-caption text-muted-foreground">{KIND_LABELS[evaluator.kind]}</span>
    </Link>
  );
}
