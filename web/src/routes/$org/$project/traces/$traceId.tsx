import { createFileRoute, Link, notFound } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import { problemOf } from "@/api/problem";
import { traceDetailQuery } from "@/api/queries";
import { useProject } from "@/components/shell/project-context";
import { TraceView } from "@/components/trace/trace-view";
import { useListReturn } from "@/lib/list-return";
import {
  parseTracePageSearch,
  parseTracesSearch,
  type TraceMode,
  type TracePageSearch,
} from "@/lib/search/traces";

export const Route = createFileRoute("/$org/$project/traces/$traceId")({
  validateSearch: (search) => parseTracePageSearch(search),
  loader: async ({ context, params }) => {
    try {
      await context.queryClient.ensureQueryData(
        traceDetailQuery(params.org, params.project, params.traceId),
      );
    } catch (error) {
      if (problemOf(error)?.status === 404) {
        throw notFound();
      }
      throw error;
    }
  },
  component: TracePage,
});

function TracePage() {
  const { traceId } = Route.useParams();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const { org, project } = useProject();
  const ret = useListReturn("traces");
  return (
    <div className="relative h-full min-h-0 flex-1">
      <div className="absolute inset-0 flex min-h-0 flex-col">
        <TraceView
          traceId={traceId}
          spanId={search.span}
          mode={search.mode}
          backLink={
            <>
              <Link
                to="/$org/$project/traces"
                params={{ org, project }}
                search={parseTracesSearch(ret?.search ?? {})}
                state={(current) => ({
                  ...current,
                  listRestore: ret
                    ? {
                        anchorId: ret.anchorId,
                        anchorOffset: ret.anchorOffset,
                        openedId: ret.openedId,
                      }
                    : undefined,
                })}
                aria-label="Back to traces"
                className="focus-ring -ml-1 inline-flex items-center gap-0.5 rounded-sm px-1 text-[12px] text-muted-foreground hover:bg-row-hover hover:text-foreground"
              >
                <ChevronLeft aria-hidden="true" className="size-3.5" />
                Traces
              </Link>
              <span aria-hidden="true" className="text-muted-foreground/60">
                /
              </span>
            </>
          }
          onModeChange={(next) => {
            void navigate({
              to: "/$org/$project/traces/$traceId",
              search: (prev) => withMode(prev, next),
              replace: true,
            });
          }}
          onSelectSpan={(spanId) => {
            void navigate({
              to: "/$org/$project/traces/$traceId",
              search: (prev) => ({ ...prev, span: spanId }),
              replace: true,
            });
          }}
          onShowSummary={() => {
            void navigate({
              to: "/$org/$project/traces/$traceId",
              search: (prev) => withoutSpan(prev),
              replace: true,
            });
          }}
        />
      </div>
    </div>
  );
}

function withoutSpan(search: TracePageSearch): TracePageSearch {
  const { span: _span, ...rest } = search;
  return rest;
}

function withMode(search: TracePageSearch, mode: TraceMode): TracePageSearch {
  const { mode: _mode, ...rest } = search;
  return mode === "tree" ? rest : { ...rest, mode };
}
