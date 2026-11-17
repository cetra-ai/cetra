import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import type { ReactNode } from "react";
import { metricsQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { KpiRow, KpiRowSkeleton } from "@/components/charts/kpi-tiles";
import { DashboardBody, DashboardBodySkeleton } from "@/components/charts/metric-chart";
import { useProject } from "@/components/shell/project-context";
import { UserMetadata, UserScores } from "@/components/user/user-chips";
import { bucketRange } from "@/lib/metrics";
import { userTraceFilter } from "@/lib/search/users";

/**
 * The user page's Overview tab (UP6): the user's scores and metadata, then the project dashboard
 * computed for this user alone. Relies on the page's `TooltipProvider`.
 */
export function UserOverview({
  user,
  from,
  to,
  includeEval,
  includeExperiments,
}: {
  user: Schema<"EndUserSummary">;
  from: string;
  to: string;
  includeEval: boolean;
  includeExperiments: boolean;
}) {
  const { org, project } = useProject();
  const navigate = useNavigate();
  const filter = userTraceFilter(user.user_id);
  const metrics = useQuery(
    metricsQuery(org, project, { from, to, filter, includeEval, includeExperiments }),
  );
  const data = metrics.data;

  // Opens the traces of one bucket of this user's dashboard.
  function openBucket(timeMs: number) {
    if (!data) {
      return;
    }
    const bucket = bucketRange(timeMs, data.bucket_ns);
    void navigate({
      to: "/$org/$project/traces",
      params: { org, project },
      search: {
        from: bucket.from,
        to: bucket.to,
        filter,
        ...(includeEval ? { eval: 1 as const } : {}),
        ...(includeExperiments ? { exp: 1 as const } : {}),
      },
    });
  }

  return (
    <div className="min-h-0 flex-1 overflow-auto p-4">
      <div className="space-y-3">
        <div className="grid grid-cols-1 gap-3 md:grid-cols-2">
          <OverviewCard label="User scores" title="Scores">
            <UserScores scores={user.scores} />
          </OverviewCard>
          <OverviewCard label="User metadata" title="Metadata">
            <UserMetadata metadata={user.metadata} />
          </OverviewCard>
        </div>
        <h2 className="text-caption text-muted-foreground uppercase">Usage</h2>
        {data ? (
          <>
            <KpiRow kpis={data.kpis} />
            <DashboardBody response={data} onBucket={openBucket} />
          </>
        ) : metrics.isError ? (
          <p className="text-body text-muted-foreground">Could not load the usage charts.</p>
        ) : (
          <>
            <KpiRowSkeleton />
            <DashboardBodySkeleton />
          </>
        )}
      </div>
    </div>
  );
}

/** A card; `label` is its accessible name, distinct from the dashboard's own "Scores" region. */
function OverviewCard({
  label,
  title,
  children,
}: {
  label: string;
  title: string;
  children: ReactNode;
}) {
  return (
    <section aria-label={label} className="rounded-md border p-3 text-body">
      <h2 className="mb-2 text-caption text-muted-foreground uppercase">{title}</h2>
      {children}
    </section>
  );
}
