import { useMatch, useNavigate, useParams } from "@tanstack/react-router";
import { ChevronsUpDown, Loader2 } from "lucide-react";
import { $api } from "@/api/client";
import { projectListInit } from "@/api/queries";
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

export function ProjectSwitcher() {
  const { org } = useParams({ from: "/$org" });
  const projectMatch = useMatch({ from: "/$org/$project", shouldThrow: false });
  const project = projectMatch?.params.project;
  const navigate = useNavigate();
  const projects = $api.useInfiniteQuery(
    "get",
    "/api/v1/orgs/{org}/projects",
    projectListInit(org),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );
  const rows = projects.data?.pages.flatMap((page) => page.data) ?? [];
  const current = rows.find((item) => item.slug === project);
  const label = current?.name ?? project ?? "Project";

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          variant="ghost"
          size="sm"
          aria-label={label}
          className="h-7 gap-1 px-1.5 font-medium"
        >
          <span className="max-w-48 truncate">{label}</span>
          <ChevronsUpDown className="size-3.5 text-muted-foreground" aria-hidden />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="min-w-48 whitespace-nowrap">
        <DropdownMenuRadioGroup
          value={project ?? ""}
          onValueChange={(slug) => {
            void navigate({
              to: "/$org/$project/traces",
              params: { org, project: slug },
              state: (current) => ({ ...current, applyDefaultView: true }),
            });
          }}
        >
          {rows.map((item) => (
            <DropdownMenuRadioItem key={item.id} value={item.slug}>
              {item.name}
            </DropdownMenuRadioItem>
          ))}
        </DropdownMenuRadioGroup>
        {projects.hasNextPage ? (
          <>
            <DropdownMenuSeparator />
            <DropdownMenuItem
              disabled={projects.isFetchingNextPage}
              onSelect={(event) => {
                event.preventDefault();
                void projects.fetchNextPage();
              }}
            >
              {projects.isFetchingNextPage ? <Loader2 className="animate-spin" /> : null}
              Load more
            </DropdownMenuItem>
          </>
        ) : null}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
