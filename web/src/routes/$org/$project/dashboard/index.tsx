import { createFileRoute } from "@tanstack/react-router";
import { parseDashboardSearch } from "@/lib/search/dashboard";

// The dashboard is drawn by the project layout, which keeps it mounted (hidden) while another
// project page is open, so coming back does not rebuild its charts. The route only owns the URL.
export const Route = createFileRoute("/$org/$project/dashboard/")({
  validateSearch: (search) => parseDashboardSearch(search),
  component: () => null,
});
