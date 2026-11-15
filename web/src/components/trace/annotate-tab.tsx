import { useQuery } from "@tanstack/react-query";
import { useId, useState } from "react";
import { problemOf } from "@/api/problem";
import { traceDetailQuery } from "@/api/queries";
import { AnnotationForm } from "@/components/annotation/annotation-form";
import { useProject } from "@/components/shell/project-context";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Skeleton } from "@/components/ui/skeleton";
import { type AnnotationTarget, targetKey } from "@/lib/annotation";

/** The Annotate tab of a span: the form for the trace or for the span, switched by a radio pair. */
export function AnnotateTab({ traceId, spanId }: { traceId: string; spanId: string }) {
  const { org, project } = useProject();
  const trace = useQuery(traceDetailQuery(org, project, traceId));
  const [kind, setKind] = useState<"trace" | "span">("trace");
  const id = useId();

  if (trace.isPending) {
    return (
      <div className="flex flex-col gap-2">
        <Skeleton className="h-4 w-2/3" />
        <Skeleton className="h-4 w-full" />
      </div>
    );
  }
  if (trace.isError) {
    return (
      <p className="text-sm">{problemOf(trace.error)?.title ?? "Could not load this trace"}</p>
    );
  }

  // The form copies the trace's session and user onto each score (D-I-18, D-I-37).
  const target: AnnotationTarget =
    kind === "trace" ? { type: "trace", traceId } : { type: "span", traceId, spanId };
  return (
    <div className="flex flex-col gap-3">
      <RadioGroup
        aria-label="Annotate"
        value={kind}
        onValueChange={(value) => {
          setKind(value === "span" ? "span" : "trace");
        }}
        className="flex gap-3"
      >
        <div className="flex items-center gap-2">
          <RadioGroupItem value="trace" id={`${id}-trace`} />
          <Label htmlFor={`${id}-trace`}>Trace</Label>
        </div>
        <div className="flex items-center gap-2">
          <RadioGroupItem value="span" id={`${id}-span`} />
          <Label htmlFor={`${id}-span`}>Span</Label>
        </div>
      </RadioGroup>
      <AnnotationForm
        key={targetKey(target)}
        target={target}
        context={{
          sessionId: trace.data.trace.session_id,
          userId: trace.data.trace.user_id,
        }}
      />
    </div>
  );
}
