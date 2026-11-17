import { keepPreviousData } from "@tanstack/react-query";
import { type ReactNode, useEffect, useState } from "react";
import { $api } from "@/api/client";
import { problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { filterMessage } from "@/components/filter-bar/logic";
import { FormError } from "@/components/form/form-error";
import { Skeleton } from "@/components/ui/skeleton";

/** How many traces or spans the filter matched in the last 24 hours (D-I-9). */
export function MatchCounter({
  org,
  project,
  target,
  filter,
  sampleRate,
}: {
  org: string;
  project: string;
  target: "trace" | "span";
  filter: string;
  /** The sample rate as a fraction, above 0 and at most 1. */
  sampleRate: number;
}) {
  // The target and the filter are debounced as one pair, so a query never pairs a new target with
  // the filter that was current before it.
  const [debounced, setDebounced] = useState({ target, filter: filter.trim() });
  useEffect(() => {
    const timer = setTimeout(() => {
      setDebounced({ target, filter: filter.trim() });
    }, 500);
    return () => {
      clearTimeout(timer);
    };
  }, [target, filter]);
  const query = $api.useQuery(
    "post",
    "/api/v1/projects/{project}/online-rules/preview",
    {
      params: { path: { project: projectRef(org, project) } },
      body: {
        target: debounced.target,
        ...(debounced.filter ? { filter: debounced.filter } : {}),
      },
    },
    { staleTime: 60_000, placeholderData: keepPreviousData },
  );
  const pending =
    debounced.target !== target || debounced.filter !== filter.trim() || query.isFetching;

  let content: ReactNode;
  if (query.error) {
    const problem = problemOf(query.error);
    content =
      problem?.code === "invalid_filter" ? (
        <FormError message={`The filter is invalid: ${filterMessage(problem)}`} />
      ) : (
        <p>The match count could not be loaded.</p>
      );
  } else if (query.data) {
    const { matched, capped } = query.data;
    const count = capped ? "More than 10,000" : matched.toLocaleString("en-US");
    const subject = target === "span" ? "span" : "trace";
    const plural = capped || matched !== 1 ? `${subject}s` : subject;
    // A capped count is a floor, so a sampled estimate built on it would be misleading.
    const estimate =
      !capped && sampleRate > 0 && sampleRate < 1 && matched > 0
        ? ` · about ${Math.round(matched * sampleRate).toLocaleString("en-US")} would be evaluated at ${Number((sampleRate * 100).toFixed(2))} %`
        : "";
    content = <p>{`${count} ${plural} matched in the last 24 hours${estimate}`}</p>;
  } else {
    content = (
      <div className="flex items-center gap-2">
        <Skeleton className="h-3 w-24" />
        <span>Counting matches…</span>
      </div>
    );
  }

  return (
    <div
      role="status"
      aria-live="polite"
      aria-busy={pending ? "true" : undefined}
      className={`text-caption text-muted-foreground transition-opacity ${pending ? "opacity-60" : ""}`}
    >
      {content}
    </div>
  );
}
