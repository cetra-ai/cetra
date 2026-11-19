import { useQuery } from "@tanstack/react-query";
import { useNavigate, useParams } from "@tanstack/react-router";
import { ChevronsUpDown, Plus } from "lucide-react";
import { type ReactNode, useState } from "react";
import { authConfigQuery } from "@/api/auth";
import { $api } from "@/api/client";
import { NewOrgDialog } from "@/components/settings/new-org-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";

export function OrgSwitcher() {
  const { org: current } = useParams({ from: "/$org" });
  const navigate = useNavigate();
  const { data: me } = $api.useSuspenseQuery("get", "/api/auth/me");
  const membership = me.orgs.find((org) => org.slug === current);
  const demo = me.orgs.find((org) => org.demo);
  const ownOrgs = me.orgs.filter((org) => !org.demo);
  const [creating, setCreating] = useState(false);
  const { data: config } = useQuery(authConfigQuery);
  // While the config loads, creating is offered. Off on a cloud cell (D-P-31).
  const createAllowed = config?.org_create_allowed ?? true;
  const createUrl = config?.org_create_url;
  let createItem: ReactNode = null;
  if (createAllowed) {
    createItem = (
      <DropdownMenuItem onSelect={() => setCreating(true)}>
        <Plus />
        New organization
      </DropdownMenuItem>
    );
  } else if (createUrl) {
    createItem = (
      <DropdownMenuItem asChild>
        <a href={createUrl} target="_blank" rel="noopener noreferrer">
          <Plus />
          New organization
        </a>
      </DropdownMenuItem>
    );
  }

  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button
            variant="ghost"
            size="sm"
            aria-label={membership?.name ?? current}
            className="h-7 gap-1 px-1.5 font-medium"
          >
            <span className="max-w-48 truncate">{membership?.name ?? current}</span>
            {membership?.demo ? <Badge variant="secondary">Demo</Badge> : null}
            <ChevronsUpDown className="size-3.5 text-muted-foreground" aria-hidden />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start" className="min-w-48 whitespace-nowrap">
          <DropdownMenuRadioGroup
            value={current}
            onValueChange={(slug) => {
              void navigate({ to: "/$org", params: { org: slug } });
            }}
          >
            {ownOrgs.map((org) => (
              <DropdownMenuRadioItem key={org.id} value={org.slug}>
                {org.name}
              </DropdownMenuRadioItem>
            ))}
            {demo === undefined ? null : (
              <>
                {ownOrgs.length === 0 ? null : <DropdownMenuSeparator />}
                <DropdownMenuRadioItem value={demo.slug}>
                  {demo.name}
                  <Badge variant="secondary">Demo</Badge>
                </DropdownMenuRadioItem>
              </>
            )}
          </DropdownMenuRadioGroup>
          {createItem === null ? null : (
            <>
              <DropdownMenuSeparator />
              {createItem}
            </>
          )}
        </DropdownMenuContent>
      </DropdownMenu>
      <NewOrgDialog open={creating} onOpenChange={setCreating} />
    </>
  );
}
