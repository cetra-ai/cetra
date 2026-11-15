import type { RangeState } from "@/lib/time-range";

/** An empty list with no filter, search, custom range, or eval/experiment toggle. */
export function tracesNeedOnboarding(
  search: { filter?: string; search?: string; eval?: 1; exp?: 1 },
  range: RangeState,
): boolean {
  return (
    search.filter === undefined &&
    search.search === undefined &&
    search.eval === undefined &&
    search.exp === undefined &&
    range.kind === "preset"
  );
}

/**
 * Onboarding replaces an empty list only when the list is unconstrained and the all-time
 * probe found no trace, or the probe failed (the state is unknown, and onboarding was the
 * old behaviour). `probeRows` is undefined while the probe loads.
 */
export function onboardingShown(
  needs: boolean,
  probeRows: number | undefined,
  probeFailed: boolean,
): boolean {
  return needs && (probeFailed || probeRows === 0);
}
