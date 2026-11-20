import { useInfiniteQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { cn } from "cn";
import { useState } from "react";
import { problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { issuesInfiniteQuery, useMergeIssue } from "@/api/queries-issues";
import type { Schema } from "@/api/types";
import { FormError } from "@/components/form/form-error";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";

const DAY_MS = 24 * 60 * 60 * 1000;

/**
 * Picks the open issue of the same source that `issue` is merged into, then merges it and opens
 * the target. The candidates are the source's open issues matching the search text, `issue`
 * itself left out.
 */
export function MergeDialog({
  org,
  project,
  issue,
  open,
  onOpenChange,
}: {
  org: string;
  project: string;
  issue: Schema<"Issue">;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const navigate = useNavigate();
  const merge = useMergeIssue(org, project);
  const [text, setText] = useState("");
  const [target, setTarget] = useState<string | undefined>();
  const [error, setError] = useState<string | undefined>();
  // The window of the counts is not shown here; the list only needs the issues themselves.
  const [period] = useState(() => {
    const now = Date.now();
    return { from: new Date(now - DAY_MS).toISOString(), to: new Date(now).toISOString() };
  });
  const q = text.trim();
  const candidates = useInfiniteQuery({
    ...issuesInfiniteQuery(org, project, {
      state: "open",
      source: issue.source_id,
      q: q || undefined,
      sort: "last_seen",
      ...period,
    }),
    enabled: open,
  });
  const rows = (candidates.data?.pages.flatMap((page) => page.data) ?? []).filter(
    (row) => row.id !== issue.id,
  );

  function close(next: boolean) {
    if (!merge.isPending) {
      setError(undefined);
      setTarget(undefined);
      setText("");
      onOpenChange(next);
    }
  }

  function confirm() {
    if (!target) {
      return;
    }
    setError(undefined);
    merge.mutate(
      {
        params: { path: { project: projectRef(org, project), id: issue.id } },
        body: { into: target },
      },
      {
        onSuccess: () => {
          onOpenChange(false);
          void navigate({
            to: "/$org/$project/issues/$issueId",
            params: { org, project, issueId: target },
          });
        },
        onError: (caught) => {
          setError(problemMessage(caught, "Could not merge the issue."));
        },
      },
    );
  }

  return (
    <Dialog open={open} onOpenChange={close}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Merge into another issue</DialogTitle>
          <DialogDescription>
            Its occurrences count on the issue you pick, and this issue is then shown as merged.
          </DialogDescription>
        </DialogHeader>
        <Input
          aria-label="Search issues"
          placeholder="Search open issues"
          value={text}
          onChange={(event) => {
            setText(event.target.value);
          }}
        />
        {candidates.isPending ? (
          <p className="text-body text-muted-foreground">Loading issues…</p>
        ) : rows.length === 0 ? (
          <p className="text-body text-muted-foreground">No other open issues match.</p>
        ) : (
          <ul
            aria-label="Issues to merge into"
            role="listbox"
            className="flex max-h-72 flex-col overflow-auto rounded-md border"
          >
            {rows.map((row) => {
              const selected = target === row.id;
              return (
                <li key={row.id} role="none">
                  <button
                    type="button"
                    role="option"
                    aria-selected={selected}
                    onClick={() => {
                      setTarget(row.id);
                    }}
                    className={cn(
                      "flex w-full flex-col items-start px-3 py-2 text-left text-body hover:bg-row-hover",
                      selected && "bg-row-hover font-medium",
                    )}
                  >
                    <span className="truncate">{row.title}</span>
                    <span className="text-caption text-muted-foreground">
                      {row.count} {row.count === 1 ? "occurrence" : "occurrences"}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        )}
        {candidates.hasNextPage ? (
          <Button
            type="button"
            variant="ghost"
            disabled={candidates.isFetchingNextPage}
            onClick={() => {
              void candidates.fetchNextPage();
            }}
          >
            Load more
          </Button>
        ) : null}
        <FormError message={error} />
        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            disabled={merge.isPending}
            onClick={() => close(false)}
          >
            Cancel
          </Button>
          <Button type="button" disabled={!target || merge.isPending} onClick={confirm}>
            Merge
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
