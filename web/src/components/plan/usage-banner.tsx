import { UpgradeButton } from "./upgrade-button";
import { useOrgEntitlements } from "./use-org-entitlements";

/**
 * This month's usage notice for an org (D-P-38): 80 % of `units_included`, or 100 %. Shown to
 * owners and admins only, by the org layout. Renders nothing at `none` or while loading.
 */
export function UsageBanner({ org }: { org: string }) {
  const { query, upgradeUrl } = useOrgEntitlements(org);
  const usage = query.data?.usage;
  if (usage === undefined || usage.notice === "none") {
    return null;
  }
  const message = usageMessage(usage.notice, usage.resets_at, query.data?.limits.units_cap);
  return (
    <div
      role="status"
      className="flex flex-wrap items-center gap-2 border-b bg-muted px-4 py-2 text-sm"
    >
      <span>{message}</span>
      <UpgradeButton url={upgradeUrl} />
    </div>
  );
}

function usageMessage(
  notice: "warning" | "reached",
  resetsAt: string,
  cap: number | undefined,
): string {
  if (notice === "warning") {
    return "This organization has used 80% of its monthly units.";
  }
  if (cap === undefined) {
    return "This organization used all units included in its plan; additional units are billed as overage.";
  }
  // The RFC 3339 date part; the server writes UTC.
  return `This organization reached its monthly units; ingest is paused until ${resetsAt.slice(0, 10)} (UTC).`;
}
