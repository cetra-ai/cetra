import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { createFileRoute, Link } from "@tanstack/react-router";
import { Bug } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { issueSourcesQuery, issuesInfiniteQuery } from "@/api/queries-issues";
import type { Schema } from "@/api/types";
import { EmptyState } from "@/components/empty-state";
import { KIND_LABELS } from "@/components/issue-sources/source-list";
import { IssueList } from "@/components/issues/issue-list";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { canManageEvaluation } from "@/lib/roles";
import {
  ISSUE_SORTS,
  ISSUE_STATE_TABS,
  type IssueSort,
  type IssueStateTab,
  type IssuesSearch,
  parseIssuesSearch,
} from "@/lib/search/issues";

const DAY_MS = 24 * 60 * 60 * 1000;
/** Quiet time after the last keystroke before the search text reaches the URL. */
const SEARCH_DEBOUNCE_MS = 300;

const STATE_LABEL: Record<IssueStateTab, string> = {
  open: "Open",
  resolved: "Resolved",
  ignored: "Ignored",
  all: "All",
};

const SORT_LABEL: Record<IssueSort, string> = {
  last_seen: "Last seen",
  first_seen: "First seen",
};

/** The kinds the list can show: every kind of source, or all of them. The filter is client-side. */
type KindFilter = "all" | Schema<"IssueSourceKind">;
const KIND_FILTERS: readonly KindFilter[] = ["all", "error", "score", "feedback", "topic"];
const KIND_FILTER_LABEL: Record<KindFilter, string> = { all: "All", ...KIND_LABELS };

export const Route = createFileRoute("/$org/$project/issues/")({
  validateSearch: (search) => parseIssuesSearch(search),
  component: IssuesPage,
});

function IssuesPage() {
  const { org, project, data } = useProject();
  const canManage = canManageEvaluation(data.role);
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  // The `count` and `users` window is the last 24 hours as the page opens, and stays fixed.
  const [period] = useState(() => {
    const now = Date.now();
    return { from: new Date(now - DAY_MS).toISOString(), to: new Date(now).toISOString() };
  });
  const state = search.state ?? "open";
  const sort = search.sort ?? "last_seen";
  const [kind, setKind] = useState<KindFilter>("all");
  const list = useInfiniteQuery(
    issuesInfiniteQuery(org, project, {
      state,
      source: search.source,
      q: search.q,
      sort,
      ...period,
    }),
  );
  const sources = useQuery(issueSourcesQuery(org, project));
  const [text, setText] = useState(search.q ?? "");
  const searchTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(searchTimer.current), []);

  if (list.error) {
    throw list.error;
  }

  const setSearch = (change: (prev: IssuesSearch) => IssuesSearch, replace = false) => {
    void navigate({
      to: "/$org/$project/issues",
      params: { org, project },
      search: (prev) => change(prev),
      replace,
    });
  };
  const openIssue = (issueId: string) => {
    void navigate({
      to: "/$org/$project/issues/$issueId",
      params: { org, project, issueId },
    });
  };

  // An issue's kind is its source's kind, looked up in the sources the page lists.
  const sourceKinds = new Map((sources.data?.data ?? []).map((source) => [source.id, source.kind]));
  const rows = (list.data?.pages.flatMap((page) => page.data) ?? []).filter(
    (issue) => kind === "all" || sourceKinds.get(issue.source_id) === kind,
  );
  const filtered =
    search.q !== undefined || search.source !== undefined || state !== "open" || kind !== "all";
  const nothing = !list.isPending && rows.length === 0;

  return (
    <div className="flex h-full min-h-0 flex-1 flex-col">
      <div className="flex shrink-0 flex-wrap items-center gap-2 border-b px-3 py-2">
        <Tabs
          value={state}
          onValueChange={(value) => {
            const next = ISSUE_STATE_TABS.find((tab) => tab === value);
            if (next) {
              setSearch((prev) => ({ ...prev, state: next === "open" ? undefined : next }));
            }
          }}
        >
          <TabsList>
            {ISSUE_STATE_TABS.map((tab) => (
              <TabsTrigger key={tab} value={tab}>
                {STATE_LABEL[tab]}
              </TabsTrigger>
            ))}
          </TabsList>
        </Tabs>
        <Select
          value={search.source ?? "all"}
          onValueChange={(value) => {
            setSearch((prev) => ({ ...prev, source: value === "all" ? undefined : value }));
          }}
        >
          <SelectTrigger aria-label="Source" className="h-7 w-44 text-body">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="all">All sources</SelectItem>
            {(sources.data?.data ?? []).map((source) => (
              <SelectItem key={source.id} value={source.id}>
                {source.name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Select
          value={kind}
          onValueChange={(value) => {
            const next = KIND_FILTERS.find((option) => option === value);
            if (next) {
              setKind(next);
            }
          }}
        >
          <SelectTrigger aria-label="Kind" className="h-7 w-36 text-body">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {KIND_FILTERS.map((option) => (
              <SelectItem key={option} value={option}>
                {KIND_FILTER_LABEL[option]}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <div className="ml-auto flex items-center gap-2">
          <Select
            value={sort}
            onValueChange={(value) => {
              const next = ISSUE_SORTS.find((option) => option === value);
              if (next) {
                setSearch((prev) => ({ ...prev, sort: next === "last_seen" ? undefined : next }));
              }
            }}
          >
            <SelectTrigger aria-label="Sort" className="h-7 w-36 text-body">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {ISSUE_SORTS.map((option) => (
                <SelectItem key={option} value={option}>
                  {SORT_LABEL[option]}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Input
            aria-label="Search issues"
            placeholder="Search issues"
            value={text}
            className="h-7 w-56 text-body"
            onChange={(event) => {
              const value = event.target.value;
              setText(value);
              clearTimeout(searchTimer.current);
              searchTimer.current = setTimeout(() => {
                setSearch((prev) => ({ ...prev, q: value.trim() || undefined }), true);
              }, SEARCH_DEBOUNCE_MS);
            }}
          />
        </div>
      </div>
      <div className="flex min-h-0 flex-1 flex-col gap-3 px-3 py-3">
        {list.isPending ? (
          <>
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
          </>
        ) : nothing && !filtered ? (
          <div className="flex flex-col items-center gap-2">
            <EmptyState
              icon={Bug}
              title="No issues yet"
              description="Add an error source to start finding issues."
            />
            {canManage ? (
              <Button asChild>
                <Link to="/$org/$project/issue-sources" params={{ org, project }}>
                  Issue sources
                </Link>
              </Button>
            ) : null}
          </div>
        ) : nothing ? (
          <p className="p-3 text-body text-muted-foreground">No issues match.</p>
        ) : (
          <IssueList
            rows={rows}
            hasNextPage={list.hasNextPage}
            isFetchingNextPage={list.isFetchingNextPage}
            fetchNextPage={() => {
              void list.fetchNextPage();
            }}
            onOpen={openIssue}
            emptyState={null}
          />
        )}
      </div>
    </div>
  );
}
