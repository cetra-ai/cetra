import { useQuery } from "@tanstack/react-query";
import { createFileRoute, notFound, Outlet } from "@tanstack/react-router";
import { meQuery } from "@/api/queries";
import { DemoBanner } from "@/components/demo-banner";
import { UsageBanner } from "@/components/plan/usage-banner";
import { TopBarLayout } from "@/components/shell/top-bar-layout";
import { atLeast } from "@/lib/roles";

export const Route = createFileRoute("/$org")({
  loader: async ({ context, params }) => {
    const me = await context.queryClient.ensureQueryData(meQuery);
    if (!me.orgs.some((org) => org.slug === params.org)) {
      throw notFound();
    }
  },
  component: OrgLayout,
});

function OrgLayout() {
  const { org } = Route.useParams();
  const me = useQuery(meQuery);
  const role = me.data?.orgs.find((entry) => entry.slug === org)?.role;
  return (
    <TopBarLayout>
      <DemoBanner org={org} />
      {atLeast(role, "admin") ? <UsageBanner org={org} /> : null}
      <Outlet />
    </TopBarLayout>
  );
}
