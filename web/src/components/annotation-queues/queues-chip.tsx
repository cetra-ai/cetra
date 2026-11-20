import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { ClipboardCheck } from "lucide-react";
import { queuesHoldingQuery } from "@/api/queries-annotation-queues";
import { useProject } from "@/components/shell/project-context";
import { MetricChip } from "@/components/trace/metric-chip";

type QueuesChipProps = { traceId: string } | { sessionId: string };

/**
 * How many queues hold an item of this trace or session. Renders nothing while loading, on error
 * or at zero. One queue links to that queue; several link to the queue list.
 */
export function QueuesChip(props: QueuesChipProps) {
  const { org, project } = useProject();
  const holding = useQuery(queuesHoldingQuery(org, project, props));
  const queues = holding.data?.data ?? [];
  if (queues.length === 0) {
    return null;
  }
  const count = queues.length;
  const text = `In ${count} ${count === 1 ? "queue" : "queues"}`;
  const className = "focus-ring rounded-sm text-foreground underline-offset-2 hover:underline";
  const only = queues.length === 1 ? queues[0] : undefined;
  return (
    <MetricChip icon={ClipboardCheck} label="Annotation queues">
      {only !== undefined ? (
        <Link
          to="/$org/$project/annotation-queues/$queueId"
          params={{ org, project, queueId: only.id }}
          className={className}
        >
          {text}
        </Link>
      ) : (
        <Link to="/$org/$project/annotation-queues" params={{ org, project }} className={className}>
          {text}
        </Link>
      )}
    </MetricChip>
  );
}
