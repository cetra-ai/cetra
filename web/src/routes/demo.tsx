import { createFileRoute, redirect } from "@tanstack/react-router";
import { meQuery, projectListOptions } from "@/api/queries";

/**
 * The website's "Try demo" link (D187): the demo org's first project by name. The root gate sends a
 * signed-out visitor to `/login?redirect=/demo` before this loader runs.
 */
export const Route = createFileRoute("/demo")({
  loader: async ({ context }) => {
    const me = await context.queryClient.ensureQueryData(meQuery);
    const demo = me.orgs.find((org) => org.demo);
    if (!demo) {
      throw redirect({ to: "/" });
    }
    const projects = await context.queryClient.ensureInfiniteQueryData(
      projectListOptions(demo.slug),
    );
    const first = projects.pages[0]?.data[0];
    if (!first) {
      throw redirect({ to: "/$org", params: { org: demo.slug } });
    }
    throw redirect({
      to: "/$org/$project/traces",
      params: { org: demo.slug, project: first.slug },
    });
  },
});
