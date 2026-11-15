import { createFileRoute, Link } from "@tanstack/react-router";
import { AuthLayout } from "@/components/auth/auth-layout";
import { Button } from "@/components/ui/button";
import { parseString } from "@/lib/search/common";

/** Where an org sign-in lands when JIT found no seat but the person keeps other orgs (D-P-56). */
export const Route = createFileRoute("/sso/no-seat")({
  validateSearch: (search) => ({ org: parseString(search.org, 64) }),
  component: NoSeatPage,
});

function NoSeatPage() {
  const { org } = Route.useSearch();
  const message = org
    ? `No seat available in ${org}; ask an owner.`
    : "No seat available in this organization; ask an owner.";
  return (
    <AuthLayout title="No seat available">
      <p role="status" className="text-sm">
        {message}
      </p>
      <Button asChild className="self-start">
        <Link to="/">Continue</Link>
      </Button>
    </AuthLayout>
  );
}
