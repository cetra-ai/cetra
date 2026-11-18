import { useNavigate } from "@tanstack/react-router";
import type { ReactNode } from "react";
import type { Schema } from "@/api/types";
import { useModelRanking } from "@/components/dashboard/use-model-ranking";
import { type BucketTarget, Widget, type WidgetContext } from "@/components/dashboard/widget";
import { WidgetGrid } from "@/components/dashboard/widget-grid";
import { useProject } from "@/components/shell/project-context";
import { UserMetadata, UserScores } from "@/components/user/user-chips";
import { USER_OVERVIEW } from "@/lib/dashboards/user-overview";
import { userTraceFilter } from "@/lib/search/users";

/**
 * The user page's Overview tab (UP6): the user's scores and metadata, then the user's usage: the
 * dashboard's numbers and charts computed for this user alone. Relies on the page's
 * `TooltipProvider`.
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
  const base: WidgetContext = {
    org,
    project,
    from,
    to,
    filter,
    includeEval,
    includeExperiments,
  };
  const ranking = useModelRanking(base);
  const context: WidgetContext = { ...base, ranking: ranking.ranking };

  // Opens the traces of one bucket of this user's usage: the bucket's range with the user's filter.
  function openBucket(target: BucketTarget) {
    void navigate({
      to: "/$org/$project/traces",
      params: { org, project },
      search: {
        from: target.from,
        to: target.to,
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
        <WidgetGrid
          widgets={USER_OVERVIEW.widgets}
          render={(widget) => <Widget widget={widget} context={context} onBucket={openBucket} />}
        />
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
