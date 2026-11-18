import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { BellRing } from "lucide-react";
import { useState } from "react";
import { alertRulesQuery } from "@/api/queries-alerts";
import { firingRules } from "@/components/shell/use-nav-badges";
import { Button } from "@/components/ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { formatRelative } from "@/lib/format";

/**
 * The count of enabled alert rules that are firing, with the list of them (D-C-20). Renders
 * nothing when none fires, or while the rules load or after they fail.
 */
export function AlertBell({ org, project }: { org: string; project: string }) {
  const [open, setOpen] = useState(false);
  const query = useQuery({ ...alertRulesQuery(org, project), refetchInterval: 60_000 });

  if (!query.data || query.isError) {
    return null;
  }
  const firing = firingRules(query.data.data);
  if (firing.length === 0) {
    return null;
  }
  const count = firing.length;
  const label = `${count} alert ${count === 1 ? "rule" : "rules"} firing`;
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          variant="ghost"
          size="sm"
          className="h-7 gap-1 px-1.5 text-destructive"
          aria-label={label}
        >
          <BellRing className="size-3.5" />
          {count}
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-72 p-0">
        <div className="border-b px-3 py-2 text-body font-medium">Firing alerts</div>
        <ul className="max-h-80 overflow-y-auto py-1">
          {firing.map((rule) => (
            <li key={rule.id}>
              <Link
                to="/$org/$project/alerts"
                params={{ org, project }}
                search={{ rule: rule.id }}
                onClick={() => setOpen(false)}
                className="flex flex-col gap-0.5 rounded-sm px-3 py-1.5 hover:bg-muted"
              >
                <span className="truncate text-body">{rule.name}</span>
                {rule.last_fired_at ? (
                  <span className="text-caption text-muted-foreground">
                    fired {formatRelative(rule.last_fired_at, Date.now())}
                  </span>
                ) : null}
              </Link>
            </li>
          ))}
        </ul>
        <div className="border-t px-3 py-2">
          <Link
            to="/$org/$project/alerts"
            params={{ org, project }}
            onClick={() => setOpen(false)}
            className="text-caption font-medium text-primary hover:underline"
          >
            All alerts
          </Link>
        </div>
      </PopoverContent>
    </Popover>
  );
}
