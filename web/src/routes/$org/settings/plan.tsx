import { createFileRoute } from "@tanstack/react-router";
import { Check, Lock, TriangleAlert } from "lucide-react";
import type { Schema } from "@/api/types";
import { type Feature, featureOffer } from "@/components/plan/feature-label";
import { UpgradeButton } from "@/components/plan/upgrade-button";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { SettingsPage } from "@/components/settings/settings-page";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";

export const Route = createFileRoute("/$org/settings/plan")({
  component: OrgPlanPage,
});

const FEATURES: Feature[] = ["retention_management", "project_roles", "sso_enforcement"];

const FEATURE_NAMES: Record<Feature, string> = {
  retention_management: "Retention management",
  project_roles: "Project roles",
  sso_enforcement: "SSO enforcement",
};

const numbers = new Intl.NumberFormat("en-US");

function formatLimit(value: number | undefined): string {
  return value === undefined ? "unlimited" : numbers.format(value);
}

function capitalise(word: string): string {
  return word.charAt(0).toUpperCase() + word.slice(1);
}

/** The date part of an RFC 3339 timestamp. */
function day(iso: string): string {
  return iso.slice(0, 10);
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
        <PlanDetails entitlements={query.data} allows={allows} upgradeUrl={upgradeUrl} />
      )}
    </SettingsPage>
  );
}

type Entitlements = Schema<"OrgEntitlements">;

function PlanDetails({
  entitlements,
  allows,
  upgradeUrl,
}: {
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
  ];

  return (
    <>
      <section aria-labelledby="plan-name" className="flex flex-col gap-2">
        <h2 id="plan-name" className="text-base font-medium">
          {name}
        </h2>
        {plan.source === "defaults" ? (
          <p className="text-sm text-muted-foreground">Instance defaults apply.</p>
        ) : null}
        {status === "grace" && expires_at !== undefined && grace_ends_at !== undefined ? (
          <p
            role="alert"
            className="flex items-center gap-2 text-sm text-amber-700 dark:text-amber-400"
          >
            <TriangleAlert className="size-4 shrink-0" aria-hidden="true" />
            This plan expired on {day(expires_at)}. It stays in effect until {day(grace_ends_at)}.
          </p>
        ) : null}
      </section>

      <section aria-labelledby="plan-features" className="flex flex-col gap-2">
        <h2 id="plan-features" className="text-base font-medium">
          Features
        </h2>
        <ul className="flex flex-col divide-y rounded-md border">
          {FEATURES.map((feature) => (
            <li key={feature} className="flex items-center justify-between gap-4 px-3 py-2 text-sm">
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
      </section>

      <section aria-labelledby="plan-limits" className="flex flex-col gap-2">
        <h2 id="plan-limits" className="text-base font-medium">
          Limits
        </h2>
        <Table>
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
      </section>

      <div>
        <UpgradeButton url={upgradeUrl} />
      </div>
    </>
  );
}
