import { createFileRoute, Link } from "@tanstack/react-router";
import { AuthLayout } from "@/components/auth/auth-layout";
import { Button } from "@/components/ui/button";
import { parseString } from "@/lib/search/common";

/** The server's org slug shape: 2-48 of `[a-z0-9-]`, no leading, trailing or doubled `-`. */
const SLUG = /^(?!.*--)[a-z0-9][a-z0-9-]{0,46}[a-z0-9]$/;

/** Where an org sign-in lands when JIT found no seat but the person keeps other orgs (D-P-56). */
export const Route = createFileRoute("/sso/no-seat")({
  validateSearch: (search) => {
    const org = parseString(search.org, 64);
    return { org: org !== undefined && SLUG.test(org) ? org : undefined };
  },
  component: NoSeatPage,
});

function NoSeatPage() {
  const { org } = Route.useSearch();
  const message = org
    ? `No seat available in ${org}; ask an owner.`
    : "No seat available in this organization; ask an owner.";
  return (
    <AuthLayout title="No seat available">
      <p role="status" className="text-body">
        {message}
      </p>
      <Button asChild className="self-start">
        <Link to="/">Continue</Link>
      </Button>
    </AuthLayout>
  );
}
