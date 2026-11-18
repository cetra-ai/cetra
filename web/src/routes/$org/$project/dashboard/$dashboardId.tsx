import { createFileRoute } from "@tanstack/react-router";
import { parseDashboardSearch } from "@/lib/search/dashboard";

// A custom dashboard is drawn by the project layout, like the Overview (see dashboard/index.tsx).
// The route only owns the URL: its id and the same search params.
export const Route = createFileRoute("/$org/$project/dashboard/$dashboardId")({
  validateSearch: (search) => parseDashboardSearch(search),
  component: () => null,
});
