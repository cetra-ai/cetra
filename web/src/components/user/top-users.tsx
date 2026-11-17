import { cn } from "cn";
import type { Schema } from "@/api/types";
import { formatCost } from "@/lib/format";
import { userIdentity } from "@/lib/user-identity";

/** The users with the highest cost in the list's scope, as buttons that open the drawer (UP-7). */
export function TopUsers({
  users,
  onSelect,
}: {
  users: readonly Schema<"EndUserSummary">[];
  onSelect: (userId: string) => void;
}) {
  const costly = users.filter((user) => (user.cost_total ?? 0) > 0);
  if (costly.length === 0) {
    return null;
  }
  const maxCost = Math.max(...costly.map((user) => user.cost_total ?? 0));
  return (
    <section
      aria-label="Top users by cost"
      className="flex h-8 shrink-0 items-center gap-2 overflow-x-auto border-b px-3 text-[12px]"
    >
      <span className="shrink-0 text-muted-foreground">Top by cost</span>
      {costly.map((user) => {
        const cost = user.cost_total ?? 0;
        const { name } = userIdentity(user.metadata);
        return (
          <button
            key={user.user_id}
            type="button"
            aria-label={`${name ?? user.user_id}: ${formatCost(cost)}`}
            onClick={() => onSelect(user.user_id)}
            className="focus-ring relative inline-flex shrink-0 items-center gap-1.5 overflow-hidden rounded-md border px-2 py-0.5"
          >
            <span
              aria-hidden
              className="absolute inset-y-0 left-0 bg-chart-1/15"
              style={{ width: `${(cost / maxCost) * 100}%` }}
            />
            <span className={cn("relative max-w-40 truncate", !name && "font-mono")}>
              {name ?? user.user_id}
            </span>
            <span className="num relative text-muted-foreground">{formatCost(cost)}</span>
          </button>
        );
      })}
    </section>
  );
}
