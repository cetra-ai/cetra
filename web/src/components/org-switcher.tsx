import { useQuery } from "@tanstack/react-query";
import { useNavigate, useParams } from "@tanstack/react-router";
import { Plus } from "lucide-react";
import { type ReactNode, useState } from "react";
import { authConfigQuery } from "@/api/auth";
import { $api } from "@/api/client";
import { NewOrgDialog } from "@/components/settings/new-org-dialog";
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
          <Button variant="outline" aria-label={membership?.name ?? current}>
            {membership?.name ?? current}
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start">
          <DropdownMenuRadioGroup
            value={current}
            onValueChange={(slug) => {
              void navigate({ to: "/$org", params: { org: slug } });
            }}
          >
            {me.orgs.map((org) => (
              <DropdownMenuRadioItem key={org.id} value={org.slug}>
                {org.name}
              </DropdownMenuRadioItem>
            ))}
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
