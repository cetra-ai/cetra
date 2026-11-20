import { useQuery } from "@tanstack/react-query";
import { createFileRoute, notFound } from "@tanstack/react-router";
import { FileText } from "lucide-react";
import { useEffect, useState } from "react";
import { $api } from "@/api/client";
import { problemOf } from "@/api/problem";
import {
  promptQuery,
  promptSettingsQuery,
  promptVersionQuery,
  promptVersionsInit,
} from "@/api/queries-prompts";
import type { Schema } from "@/api/types";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { EmptyState } from "@/components/empty-state";
import { LabelsTab } from "@/components/prompts/labels-tab";
import { MetricsTab } from "@/components/prompts/metrics-tab";
import { LATEST, LOAD_MORE, PromptHeader } from "@/components/prompts/prompt-header";
import { TemplateTab } from "@/components/prompts/template-view";
import { VersionsTab } from "@/components/prompts/versions-tab";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { type PromptTab, parsePromptSearch, withPromptTab, withPromptVersion } from "@/lib/prompts";
import { atLeast, canManagePrompts } from "@/lib/roles";

export const Route = createFileRoute("/$org/$project/prompts/$prompt")({
  validateSearch: parsePromptSearch,
  loader: async ({ context, params }) => {
    try {
      await context.queryClient.ensureQueryData(
        promptQuery(params.org, params.project, params.prompt),
      );
    } catch (error) {
      if (problemOf(error)?.status === 404) {
        throw notFound();
      }
      throw error;
    }
  },
  component: PromptPage,
});

function PromptPage() {
  const { org, project } = useProject();
  const { prompt: name } = Route.useParams();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const detail = useQuery(promptQuery(org, project, name));
  const latest = detail.data?.latest_version;
  // A version at or above the latest one is the latest version, so the URL drops it.
  const redundant =
    search.version !== undefined && latest !== undefined && search.version >= latest;
  useEffect(() => {
    if (redundant) {
      void navigate({
        to: "/$org/$project/prompts/$prompt",
        params: { org, project, prompt: name },
        search: (previous) => withPromptVersion(previous, undefined),
        replace: true,
        state: true,
      });
    }
  }, [redundant, navigate, org, project, name]);

  if (detail.isPending) {
    return null;
  }
  if (detail.isError) {
    throw detail.error;
  }
  const prompt = detail.data;
  // A version named in the URL below the latest one is read; otherwise the latest is shown.
  const pinned =
    search.version !== undefined && search.version < prompt.latest_version
      ? search.version
      : undefined;
  return <PromptContent prompt={prompt} pinned={pinned} tab={search.tab ?? "template"} />;
}

function PromptContent({
  prompt,
  pinned,
  tab,
}: {
  prompt: Schema<"Prompt">;
  pinned: number | undefined;
  tab: PromptTab;
}) {
  const { org, project, data } = useProject();
  const navigate = Route.useNavigate();
  const canManage = canManagePrompts(data.role);
  const canProtect = atLeast(data.role, "admin");
  const settings = useQuery(promptSettingsQuery(org, project));
  const protectedLabels = settings.data?.protected_labels ?? [];
  const name = prompt.name;
  const latest = prompt.latest_version;
  const shown = pinned ?? latest;
  const atLatest = shown === latest;
  const versions = $api.useInfiniteQuery(
    "get",
    "/api/v1/projects/{project}/prompts/{prompt}/versions",
    promptVersionsInit(org, project, name),
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
    },
  );
  const version = useQuery(promptVersionQuery(org, project, name, shown));
  // Set by the template tab while its edit has unsaved changes.
  const [dirty, setDirty] = useState(false);
  // The navigation that waits for the user to confirm dropping the unsaved edit.
  const [leaving, setLeaving] = useState<(() => void) | undefined>();

  if (versions.error) {
    throw versions.error;
  }
  // A version that does not exist is shown as missing; other failures are the error page's.
  if (version.isError && problemOf(version.error)?.status !== 404) {
    throw version.error;
  }

  const loaded = versions.data?.pages.flatMap((page) => page.data) ?? [];
  const missing = version.isError;
  const labels = version.data?.labels ?? [];

  // Shows another version (undefined = the latest), keeping the tab unless one is given. With an
  // unsaved edit in the template tab, the user confirms first: the edit is dropped by the change.
  function goTo(next: number | undefined, nextTab?: PromptTab) {
    const run = () => {
      void navigate({
        to: "/$org/$project/prompts/$prompt",
        params: { org, project, prompt: name },
        search: (previous) => {
          const moved = withPromptVersion(previous, next);
          return nextTab === undefined ? moved : withPromptTab(moved, nextTab);
        },
        state: true,
      });
    };
    if (dirty && (next ?? latest) !== shown) {
      setLeaving(() => run);
      return;
    }
    run();
  }

  function chooseVersion(value: string) {
    if (value === LOAD_MORE) {
      void versions.fetchNextPage();
      return;
    }
    goTo(value === LATEST ? undefined : Number(value));
  }

  function chooseTab(value: string) {
    const next = parsePromptSearch({ tab: value }).tab;
    if (next === undefined) {
      return;
    }
    void navigate({
      to: "/$org/$project/prompts/$prompt",
      params: { org, project, prompt: name },
      search: (previous) => withPromptTab(previous, next),
      state: true,
    });
  }

  return (
    <div className="flex h-full min-h-0 w-full flex-1 flex-col">
      <PromptHeader
        prompt={prompt}
        shown={shown}
        versions={loaded}
        hasNextPage={versions.hasNextPage}
        labels={labels}
        canManage={canManage}
        canProtect={canProtect}
        protectedLabels={protectedLabels}
        onChooseVersion={chooseVersion}
      />

      {atLatest ? null : (
        <div
          role="status"
          className="flex flex-wrap items-center justify-between gap-2 border-b bg-muted/40 px-3 py-1 text-body"
        >
          <p>{`Viewing version ${shown}.`}</p>
          <Button
            type="button"
            variant="outline"
            size="xs"
            onClick={() => {
              chooseVersion(LATEST);
            }}
          >
            Go to latest
          </Button>
        </div>
      )}

      {missing ? (
        <div className="flex flex-col items-center gap-3">
          <EmptyState icon={FileText} title={`Version ${shown} does not exist.`} />
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={() => {
              chooseVersion(LATEST);
            }}
          >
            Go to latest
          </Button>
        </div>
      ) : (
        <Tabs value={tab} onValueChange={chooseTab} className="flex min-h-0 flex-1 flex-col">
          <TabsList variant="line" className="h-8 shrink-0 px-3">
            <TabsTrigger value="template" className="text-body">
              Template
            </TabsTrigger>
            <TabsTrigger value="versions" className="text-body">
              Versions
            </TabsTrigger>
            <TabsTrigger value="labels" className="text-body">
              Labels
            </TabsTrigger>
            <TabsTrigger value="metrics" className="text-body">
              Metrics
            </TabsTrigger>
          </TabsList>
          {/* Kept mounted while another tab is open, so an edit in progress survives a tab change. */}
          <TabsContent
            value="template"
            forceMount
            className="min-h-0 flex-1 overflow-auto px-3 pt-3 pb-3"
          >
            {version.data ? (
              // Keyed by version: an edit belongs to the version it started from.
              <TemplateTab
                key={version.data.version}
                prompt={prompt}
                version={version.data}
                canManage={canManage}
                canProtect={canProtect}
                protectedLabels={protectedLabels}
                onDirtyChange={setDirty}
              />
            ) : (
              <Skeleton className="h-24 w-full" />
            )}
          </TabsContent>
          <TabsContent value="versions" className="min-h-0 flex-1 overflow-auto px-3 pt-3 pb-3">
            <VersionsTab
              prompt={prompt}
              shown={shown}
              versions={loaded}
              hasNextPage={versions.hasNextPage}
              loadingMore={versions.isFetchingNextPage}
              onLoadMore={() => {
                void versions.fetchNextPage();
              }}
              canManage={canManage}
              canProtect={canProtect}
              protectedLabels={protectedLabels}
              onShow={(number) => {
                goTo(number === latest ? undefined : number, "template");
              }}
            />
          </TabsContent>
          <TabsContent value="labels" className="min-h-0 flex-1 overflow-auto px-3 pt-3 pb-3">
            <LabelsTab prompt={name} />
          </TabsContent>
          <TabsContent value="metrics" className="min-h-0 flex-1 overflow-auto px-3 pt-3 pb-3">
            <MetricsTab prompt={name} />
          </TabsContent>
        </Tabs>
      )}

      <ConfirmDialog
        open={leaving !== undefined}
        onOpenChange={(open) => {
          if (!open) {
            setLeaving(undefined);
          }
        }}
        title="Discard your edits?"
        description="Showing another version drops the changes you have not saved to this template."
        confirmLabel="Discard"
        destructive
        pending={false}
        onConfirm={() => {
          const run = leaving;
          setLeaving(undefined);
          setDirty(false);
          run?.();
        }}
      />
    </div>
  );
}
