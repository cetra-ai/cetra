import { useQuery } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { Check, Lock, TriangleAlert } from "lucide-react";
import { Bar, BarChart, CartesianGrid, Legend, XAxis, YAxis } from "recharts";
import { orgUsageQuery } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { type Feature, featureOffer } from "@/components/plan/feature-label";
import { UpgradeButton } from "@/components/plan/upgrade-button";
import { day, numbers, UsageMeter } from "@/components/plan/usage-meter";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { SettingsCard } from "@/components/settings/settings-card";
import { SettingsPage } from "@/components/settings/settings-page";
import {
  type ChartConfig,
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
} from "@/components/ui/chart";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { formatBytes, formatCost } from "@/lib/format";

export const Route = createFileRoute("/$org/settings/plan")({
  component: OrgPlanPage,
});

const FEATURES: Feature[] = ["retention_management", "project_roles", "sso_enforcement", "org_sso"];

const FEATURE_NAMES: Record<Feature, string> = {
  retention_management: "Retention management",
  project_roles: "Project roles",
  sso_enforcement: "SSO enforcement",
  org_sso: "Single sign-on",
};

const USAGE_CHART: ChartConfig = {
  spans: { label: "Spans", color: "var(--chart-1)" },
  scores: { label: "Scores", color: "var(--chart-2)" },
};

function formatLimit(value: number | undefined): string {
  return value === undefined ? "unlimited" : numbers.format(value);
}

function capitalise(word: string): string {
  return word.charAt(0).toUpperCase() + word.slice(1);
}

function OrgPlanPage() {
  const { org } = Route.useParams();
  const { query, allows, upgradeUrl } = useOrgEntitlements(org);

  if (query.error) {
    throw query.error;
  }

  return (
    <SettingsPage title="Plan" description="What this organization's plan includes.">
      {query.data === undefined ? (
        <>
          <Skeleton className="h-16 w-full" />
          <Skeleton className="h-48 w-full" />
        </>
      ) : (
        <PlanDetails org={org} entitlements={query.data} allows={allows} upgradeUrl={upgradeUrl} />
      )}
    </SettingsPage>
  );
}

type Entitlements = Schema<"OrgEntitlements">;

function PlanDetails({
  org,
  entitlements,
  allows,
  upgradeUrl,
}: {
  org: string;
  entitlements: Entitlements;
  allows: (feature: Feature) => boolean;
  upgradeUrl: string | undefined;
}) {
  const { plan, limits } = entitlements;
  const name = [plan.name, ...plan.add_ons.map(capitalise)].join(" + ");
  const { status, expires_at, grace_ends_at } = plan;

  const limitRows: [string, string][] = [
    ["Units per month (included)", formatLimit(limits.units_included)],
    ["Monthly cap", formatLimit(limits.units_cap)],
    ["Ingest rate (spans/s)", formatLimit(limits.ingest_spans_per_sec)],
    ["Ingest burst", formatLimit(limits.ingest_burst)],
    [
      "Data access",
      limits.data_access_days === undefined
        ? "unlimited"
        : `${numbers.format(limits.data_access_days)} days`,
    ],
    ["Seats", formatLimit(limits.max_seats)],
    ["Projects", formatLimit(limits.max_projects)],
    ["Alert rules", formatLimit(limits.max_alert_rules)],
    ["Active online evaluation rules", formatLimit(limits.max_active_online_rules)],
    ["Annotation queues", formatLimit(limits.max_annotation_queues)],
  ];

  return (
    <>
      <SettingsCard
        title={name}
        description={plan.source === "defaults" ? "Instance defaults apply." : undefined}
        actions={<UpgradeButton url={upgradeUrl} />}
      >
        {status === "grace" && expires_at !== undefined && grace_ends_at !== undefined ? (
          <p role="alert" className="flex items-center gap-2 text-body text-status-warn">
            <TriangleAlert className="size-4 shrink-0" aria-hidden="true" />
            This plan expired on {day(expires_at)}. It stays in effect until {day(grace_ends_at)}.
          </p>
        ) : null}
      </SettingsCard>

      <UsageSection org={org} entitlements={entitlements} />

      <SettingsCard title="Features">
        <ul className="flex flex-col divide-y rounded-md border">
          {FEATURES.map((feature) => (
            <li
              key={feature}
              className="flex items-center justify-between gap-4 px-3 py-2 text-body"
            >
              <span>{FEATURE_NAMES[feature]}</span>
              {allows(feature) ? (
                <span className="flex items-center gap-1 text-muted-foreground">
                  <Check className="size-4" aria-hidden="true" />
                  Included
                </span>
              ) : (
                <span className="flex items-center gap-1 text-muted-foreground">
                  <Lock className="size-4" aria-hidden="true" />
                  {featureOffer(feature)}
                </span>
              )}
            </li>
          ))}
        </ul>
      </SettingsCard>

      <SettingsCard title="Limits">
        <Table aria-label="Limits">
          <TableHeader>
            <TableRow>
              <TableHead>Limit</TableHead>
              <TableHead className="text-right">Value</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {limitRows.map(([label, value]) => (
              <TableRow key={label}>
                <TableCell>{label}</TableCell>
                <TableCell className="text-right tabular-nums">{value}</TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </SettingsCard>
    </>
  );
}

function UsageSection({ org, entitlements }: { org: string; entitlements: Entitlements }) {
  const { usage, limits } = entitlements;
  const query = useQuery(orgUsageQuery(org));

  if (query.error) {
    throw query.error;
  }

  return (
    <SettingsCard title="Usage this month">
      <UsageMeter
        units={usage.units}
        resetsAt={usage.resets_at}
        included={limits.units_included}
        cap={limits.units_cap}
      />
      {query.data === undefined ? (
        <Skeleton className="h-48 w-full" />
      ) : query.data.days.length === 0 ? (
        <p className="text-body text-muted-foreground">No usage this month.</p>
      ) : (
        <>
          <ChartContainer
            config={USAGE_CHART}
            role="img"
            aria-label="Daily units"
            className="aspect-auto h-64 w-full"
          >
            <BarChart data={query.data.days} margin={{ top: 8, right: 8, bottom: 0, left: 0 }}>
              <CartesianGrid vertical={false} />
              <XAxis dataKey="day" tickLine={false} axisLine={false} tickMargin={8} />
              <YAxis tickLine={false} axisLine={false} width={56} />
              <ChartTooltip content={<ChartTooltipContent />} />
              <Legend />
              <Bar dataKey="spans" stackId="units" fill="var(--color-spans)" />
              <Bar dataKey="scores" stackId="units" fill="var(--color-scores)" />
            </BarChart>
          </ChartContainer>
          <UsageTotals days={query.data.days} />
        </>
      )}
    </SettingsCard>
  );
}

function UsageTotals({ days }: { days: Schema<"OrgUsage">["days"] }) {
  const totals = days.reduce(
    (sum, row) => ({
      spans: sum.spans + row.spans,
      scores: sum.scores + row.scores,
      units: sum.units + row.units,
      bytes: sum.bytes + row.bytes,
      cost: sum.cost + row.judge_cost_usd,
    }),
    { spans: 0, scores: 0, units: 0, bytes: 0, cost: 0 },
  );

  return (
    <Table aria-label="Usage totals">
      <TableHeader>
        <TableRow>
          <TableHead className="text-right">Spans</TableHead>
          <TableHead className="text-right">Scores</TableHead>
          <TableHead className="text-right">Units</TableHead>
          <TableHead className="text-right">Bytes</TableHead>
          <TableHead className="text-right">Judge cost</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        <TableRow>
          <TableCell className="text-right tabular-nums">{numbers.format(totals.spans)}</TableCell>
          <TableCell className="text-right tabular-nums">{numbers.format(totals.scores)}</TableCell>
          <TableCell className="text-right tabular-nums">{numbers.format(totals.units)}</TableCell>
          <TableCell className="text-right tabular-nums">{formatBytes(totals.bytes)}</TableCell>
          <TableCell className="text-right tabular-nums">{formatCost(totals.cost)}</TableCell>
        </TableRow>
      </TableBody>
    </Table>
  );
}
