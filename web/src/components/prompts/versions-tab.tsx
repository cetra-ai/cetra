import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { promptVersionQuery } from "@/api/queries-prompts";
import type { Schema } from "@/api/types";
import { CreatedBy } from "@/components/created-by";
import { DisabledReason } from "@/components/disabled-reason";
import { LabelChip } from "@/components/prompts/label-chip";
import { LineDiffView } from "@/components/prompts/line-diff-view";
import { SetLabelDialog } from "@/components/prompts/set-label-dialog";
import { RelativeTime } from "@/components/relative-time";
import { useProject } from "@/components/shell/project-context";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { prettyJson } from "@/lib/datasets";
import { templateText, VIEWER_REASON, withPromptTab, withPromptVersion } from "@/lib/prompts";

type PromptVersion = Schema<"PromptVersion">;

/**
 * The versions of a prompt, newest first. Each row can be shown in the template tab, compared
 * with the version before it (Diff) and get a label (Set label…). The diff is of the template,
 * then of the config.
 */
export function VersionsTab({
  prompt,
  shown,
  versions,
  hasNextPage,
  loadingMore,
  onLoadMore,
  canManage,
  canProtect,
  protectedLabels,
}: {
  prompt: Schema<"Prompt">;
  /** The version shown on the page. */
  shown: number;
  /** The loaded versions, newest first. */
  versions: PromptVersion[];
  hasNextPage: boolean;
  loadingMore: boolean;
  onLoadMore: () => void;
  canManage: boolean;
  canProtect: boolean;
  protectedLabels: string[];
}) {
  const { org, project } = useProject();
  const navigate = useNavigate();
  const [picked, setPicked] = useState<number | undefined>();
  const [labelling, setLabelling] = useState<number | undefined>();
  const selected = picked ?? shown;
  const viewerReason = canManage ? undefined : VIEWER_REASON;
  const labelSuggestions = [
    ...new Set([...prompt.labels.map((item) => item.label), ...protectedLabels]),
  ];

  function show(number: number) {
    void navigate({
      to: "/$org/$project/prompts/$prompt",
      params: { org, project, prompt: prompt.name },
      search: (previous) =>
        withPromptTab(
          withPromptVersion(previous, number === prompt.latest_version ? undefined : number),
          "template",
        ),
      state: true,
    });
  }

  return (
    <div className="flex flex-col gap-4">
      <ol aria-label="Versions" className="flex flex-col gap-2">
        {versions.map((version) => (
          <li
            key={version.version}
            data-selected={version.version === selected ? "true" : undefined}
            className="flex flex-col gap-2 rounded-md border p-3"
          >
            <div className="flex flex-wrap items-center gap-2">
              <span className="num font-medium">{`v${version.version}`}</span>
              {version.labels.map((label) =>
                label === "latest" ? (
                  <Badge key={label} variant="outline">
                    latest
                  </Badge>
                ) : (
                  <LabelChip
                    key={label}
                    label={label}
                    protectedLabel={protectedLabels.includes(label)}
                  />
                ),
              )}
              <span className="min-w-0 truncate text-body text-muted-foreground">
                {version.commit_message ?? "No commit message"}
              </span>
            </div>
            <div className="flex flex-wrap items-center gap-2">
              <div className="flex flex-wrap gap-2">
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={() => {
                    show(version.version);
                  }}
                >
                  Show
                </Button>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={() => {
                    setPicked(version.version);
                  }}
                >
                  Diff
                </Button>
                <DisabledReason reason={viewerReason}>
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    disabled={viewerReason !== undefined}
                    onClick={() => {
                      setLabelling(version.version);
                    }}
                  >
                    Set label…
                  </Button>
                </DisabledReason>
              </div>
              <span className="ml-auto text-caption text-muted-foreground">
                <CreatedBy createdBy={version.created_by} />
                {" · "}
                <RelativeTime value={version.created_at} />
              </span>
            </div>
          </li>
        ))}
      </ol>
      {hasNextPage ? (
        <div>
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={loadingMore}
            onClick={onLoadMore}
          >
            Load more versions
          </Button>
        </div>
      ) : null}

      <section aria-label="Changes" className="flex flex-col gap-3">
        <h3 className="text-ui font-medium">{`Changes in v${selected}`}</h3>
        <VersionDiff name={prompt.name} version={selected} loaded={versions} />
      </section>

      <SetLabelDialog
        open={labelling !== undefined}
        onOpenChange={(open) => {
          if (!open) {
            setLabelling(undefined);
          }
        }}
        prompt={prompt.name}
        version={labelling}
        labels={labelSuggestions}
        protectedLabels={protectedLabels}
        canProtect={canProtect}
      />
    </div>
  );
}

/**
 * The version's template and config against the version before it. A version that is not loaded
 * yet is fetched.
 */
function VersionDiff({
  name,
  version,
  loaded,
}: {
  name: string;
  version: number;
  loaded: PromptVersion[];
}) {
  const current = useVersionOf(name, version, loaded);
  const previous = useVersionOf(name, version > 1 ? version - 1 : undefined, loaded);
  if (version === 1) {
    return <p className="text-body text-muted-foreground">First version: nothing to compare.</p>;
  }
  if (current === undefined || previous === undefined) {
    return <Skeleton className="h-24 w-full" />;
  }
  return (
    <div className="flex flex-col gap-3">
      <LineDiffView
        before={templateText(previous.template, previous.type)}
        after={templateText(current.template, current.type)}
        label="Template diff"
      />
      <LineDiffView
        before={prettyJson(previous.config)}
        after={prettyJson(current.config)}
        label="Config diff"
      />
    </div>
  );
}

/** A version from the loaded list, or else fetched; `undefined` while it loads or with no number. */
function useVersionOf(
  name: string,
  number: number | undefined,
  loaded: PromptVersion[],
): PromptVersion | undefined {
  const { org, project } = useProject();
  const found = number === undefined ? undefined : loaded.find((item) => item.version === number);
  const query = useQuery({
    ...promptVersionQuery(org, project, name, number ?? 1),
    enabled: number !== undefined && found === undefined,
  });
  if (query.error) {
    throw query.error;
  }
  return found ?? query.data;
}
