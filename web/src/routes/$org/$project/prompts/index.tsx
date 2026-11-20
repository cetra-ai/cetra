import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { FileText } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { $api } from "@/api/client";
import { promptSettingsQuery, promptsInit } from "@/api/queries-prompts";
import { DataTable } from "@/components/data-table/data-table";
import { DisabledReason } from "@/components/disabled-reason";
import { EmptyState } from "@/components/empty-state";
import { NewPromptDialog } from "@/components/prompts/new-prompt-dialog";
import { PROMPT_COLUMNS } from "@/components/prompts/prompt-columns";
import { ProtectedLabelsDialog } from "@/components/prompts/protected-labels-dialog";
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
import { useBackState } from "@/lib/list-return";
import {
  DEFAULT_PROTECTED_LABELS,
  labelChoices,
  parsePromptsSearch,
  tagChoices,
} from "@/lib/prompts";
import { atLeast, canManagePrompts } from "@/lib/roles";

export const Route = createFileRoute("/$org/$project/prompts/")({
  validateSearch: parsePromptsSearch,
  component: PromptsPage,
});

const SEARCH_DEBOUNCE_MS = 300;
/** The sentinel value of the label and tag selects for "no filter". */
const ALL = "all";
const VIEWER_REASON = "Only members can edit prompts";
const SETTINGS_REASON = "Only project admins can change protected labels";

function PromptsPage() {
  const { org, project, data } = useProject();
  const search = Route.useSearch();
  const { q, label, tag, create } = search;
  const navigate = Route.useNavigate();
  const backState = useBackState("Prompts");
  const canManage = canManagePrompts(data.role);
  const canProtect = atLeast(data.role, "admin");
  const [text, setText] = useState(q ?? "");
  const [protecting, setProtecting] = useState(false);
  // True from a keystroke until the text reaches the URL, so the URL does not overwrite it.
  const typing = useRef(false);
  // Every label seen in the loaded rows during this page's life, so a filter keeps its choices.
  const seenLabels = useRef(new Set<string>());

  // The search text follows the URL when it changes from elsewhere (back, another link), unless
  // the user is typing.
  useEffect(() => {
    if (typing.current) {
      return;
    }
    setText((current) => (current.trim() === (q ?? "") ? current : (q ?? "")));
  }, [q]);

  // The search text reaches the URL after a pause, so typing does not request on every key.
  useEffect(() => {
    const timer = setTimeout(() => {
      typing.current = false;
      const next = text.trim() || undefined;
      if (next !== q) {
        void navigate({
          to: "/$org/$project/prompts",
          params: { org, project },
          search: (prev) => ({ ...prev, q: next }),
          replace: true,
        });
      }
    }, SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [text, q, navigate, org, project]);

  const query = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/prompts",
    promptsInit(org, project, { q, label, tag }),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
      placeholderData: keepPreviousData,
    },
  );
  const settings = useQuery(promptSettingsQuery(org, project));

  if (query.error) {
    throw query.error;
  }

  const rows = useMemo(() => query.data?.pages.flatMap((page) => page.data) ?? [], [query.data]);
  for (const row of rows) {
    for (const item of row.labels) {
      seenLabels.current.add(item.label);
    }
  }
  const protectedLabels = settings.data?.protected_labels ?? DEFAULT_PROTECTED_LABELS;
  const labels = labelChoices([...seenLabels.current], protectedLabels, label);
  const tags = tagChoices(rows, tag);
  const filtered = q !== undefined || label !== undefined || tag !== undefined;
  const fetchNextPage = useCallback(() => {
    void query.fetchNextPage();
  }, [query.fetchNextPage]);

  function setLabel(value: string) {
    void navigate({
      to: "/$org/$project/prompts",
      params: { org, project },
      search: (prev) => ({ ...prev, label: value === ALL ? undefined : value }),
      replace: true,
    });
  }

  function setTag(value: string) {
    void navigate({
      to: "/$org/$project/prompts",
      params: { org, project },
      search: (prev) => ({ ...prev, tag: value === ALL ? undefined : value }),
      replace: true,
    });
  }

  function openCreate() {
    void navigate({
      to: "/$org/$project/prompts",
      params: { org, project },
      search: (prev) => ({ ...prev, create: true as const }),
    });
  }

  function closeCreate() {
    void navigate({
      to: "/$org/$project/prompts",
      params: { org, project },
      search: (prev) => ({ ...prev, create: undefined }),
      replace: true,
    });
  }

  function openPrompt(name: string) {
    void navigate({
      to: "/$org/$project/prompts/$prompt",
      params: { org, project, prompt: name },
      state: backState,
    });
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex min-h-9 shrink-0 flex-wrap items-center gap-2 border-b px-3 py-1.5">
        <Input
          aria-label="Search prompts"
          placeholder="Search by name"
          value={text}
          onChange={(event) => {
            typing.current = true;
            setText(event.target.value);
          }}
          className="h-7 max-w-xs text-body"
        />
        <Select value={label ?? ALL} onValueChange={setLabel}>
          <SelectTrigger aria-label="Label" className="h-7 w-44 text-body">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={ALL}>All labels</SelectItem>
            {labels.map((item) => (
              <SelectItem key={item} value={item}>
                {item}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Select value={tag ?? ALL} onValueChange={setTag}>
          <SelectTrigger aria-label="Tag" className="h-7 w-44 text-body">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={ALL}>All tags</SelectItem>
            {tags.map((item) => (
              <SelectItem key={item} value={item}>
                {item}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <div className="ml-auto flex items-center gap-2">
          <DisabledReason reason={canProtect ? undefined : SETTINGS_REASON}>
            <Button
              type="button"
              variant="outline"
              size="sm"
              disabled={!canProtect}
              onClick={() => {
                setProtecting(true);
              }}
            >
              Protected labels
            </Button>
          </DisabledReason>
          <DisabledReason reason={canManage ? undefined : VIEWER_REASON}>
            <Button type="button" size="sm" disabled={!canManage} onClick={openCreate}>
              New prompt
            </Button>
          </DisabledReason>
        </div>
      </div>
      {query.isPending ? (
        <div className="flex flex-col gap-2 p-3">
          <Skeleton className="h-7 w-full" />
          <Skeleton className="h-7 w-full" />
          <Skeleton className="h-7 w-full" />
        </div>
      ) : rows.length === 0 ? (
        filtered ? (
          <EmptyState icon={FileText} title="No prompt matches these filters" />
        ) : (
          <EmptyState
            icon={FileText}
            title="No prompts yet"
            description="Create a prompt here, or with client.prompts.create in the SDK."
          />
        )
      ) : (
        <div className="flex min-h-0 flex-1 flex-col">
          <DataTable
            tableId="prompts"
            ariaLabel="Prompts"
            columns={PROMPT_COLUMNS}
            rows={rows}
            getRowId={(row) => row.id}
            onActivate={(row) => {
              openPrompt(row.name);
            }}
            hasNextPage={query.hasNextPage}
            isFetchingNextPage={query.isFetchingNextPage}
            fetchNextPage={fetchNextPage}
            emptyState={null}
          />
        </div>
      )}
      <NewPromptDialog
        open={create === true && canManage}
        onOpenChange={(next) => {
          if (!next) {
            closeCreate();
          }
        }}
      />
      <ProtectedLabelsDialog open={protecting} onOpenChange={setProtecting} />
    </div>
  );
}
