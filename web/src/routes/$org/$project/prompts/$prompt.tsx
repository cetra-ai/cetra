import { useQuery } from "@tanstack/react-query";
import { createFileRoute, notFound } from "@tanstack/react-router";
import { FileText } from "lucide-react";
import { useEffect } from "react";
import { $api } from "@/api/client";
import { problemOf } from "@/api/problem";
import {
  promptQuery,
  promptSettingsQuery,
  promptVersionQuery,
  promptVersionsInit,
} from "@/api/queries-prompts";
import type { Schema } from "@/api/types";
import { EmptyState } from "@/components/empty-state";
import { LabelsTab } from "@/components/prompts/labels-tab";
import { LATEST, LOAD_MORE, PromptHeader } from "@/components/prompts/prompt-header";
import { TemplateTab } from "@/components/prompts/template-view";
import { VersionsTab } from "@/components/prompts/versions-tab";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  type PromptTab,
  parsePromptSearch,
  withPromptTab,
  withPromptVersion,
} from "@/lib/prompts";
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

  function chooseVersion(value: string) {
    if (value === LOAD_MORE) {
      void versions.fetchNextPage();
      return;
    }
    const next = value === LATEST ? undefined : Number(value);
    void navigate({
      to: "/$org/$project/prompts/$prompt",
      params: { org, project, prompt: name },
      search: (previous) => withPromptVersion(previous, next),
      state: true,
    });
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
            />
          </TabsContent>
          <TabsContent value="labels" className="min-h-0 flex-1 overflow-auto px-3 pt-3 pb-3">
            <LabelsTab prompt={name} />
          </TabsContent>
        </Tabs>
      )}
    </div>
  );
}
