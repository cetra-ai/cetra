import { useQuery } from "@tanstack/react-query";
import { cn } from "cn";
import { Users } from "lucide-react";
import { useState } from "react";
import { meQuery, savedQueriesQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { useProject } from "@/components/shell/project-context";
import { Input } from "@/components/ui/input";

/** The project's saved SQL queries, split into the user's own and the shared ones. */
export function SavedQueriesList({
  viewId,
  onLoad,
}: {
  /** `search.view`: the loaded query. */
  viewId: string | undefined;
  /** Load a saved query into the console. */
  onLoad: (view: Schema<"SavedView">) => void;
}) {
  const { org, project } = useProject();
  const views = useQuery(savedQueriesQuery(org, project));
  const me = useQuery(meQuery);
  const [filter, setFilter] = useState("");

  if (views.isError || me.isError) {
    return (
      <p role="alert" className="text-body text-destructive">
        Saved queries could not be loaded.
      </p>
    );
  }
  if (views.data === undefined || me.data === undefined) {
    return <p className="text-body text-muted-foreground">Loading…</p>;
  }

  const all = views.data.data;
  const needle = filter.trim().toLowerCase();
  const matching = all
    .filter((view) => view.name.toLowerCase().includes(needle))
    .sort((a, b) => a.name.localeCompare(b.name));
  const mine = matching.filter((view) => view.owner_user_id === me.data.user.id);
  const shared = matching.filter((view) => view.owner_user_id !== me.data.user.id);

  return (
    <div className="flex flex-col gap-2">
      <Input
        aria-label="Filter saved queries"
        placeholder="Filter saved queries"
        className="h-7 text-body md:text-body"
        value={filter}
        onChange={(event) => setFilter(event.target.value)}
      />
      {all.length === 0 ? (
        <p className="text-body text-muted-foreground">
          No saved queries yet. Save one with Saved queries → Save as…
        </p>
      ) : matching.length === 0 ? (
        <p className="text-body text-muted-foreground">No match</p>
      ) : (
        <>
          <Group title="Mine" views={mine} viewId={viewId} onLoad={onLoad} />
          <Group title="Shared" views={shared} viewId={viewId} onLoad={onLoad} />
        </>
      )}
    </div>
  );
}

function Group({
  title,
  views,
  viewId,
  onLoad,
}: {
  title: string;
  views: readonly Schema<"SavedView">[];
  viewId: string | undefined;
  onLoad: (view: Schema<"SavedView">) => void;
}) {
  if (views.length === 0) {
    return null;
  }
  return (
    <section className="flex flex-col gap-0.5">
      <h3 className="px-2 text-caption text-muted-foreground">{title}</h3>
      {views.map((view) => {
        const loaded = view.id === viewId;
        return (
          <button
            key={view.id}
            type="button"
            title={view.name}
            aria-current={loaded ? "true" : undefined}
            className={cn(
              "flex w-full items-center gap-1.5 rounded px-2 py-1 text-left text-body hover:bg-muted",
              loaded && "bg-muted font-medium",
            )}
            onClick={() => onLoad(view)}
          >
            <span className="min-w-0 flex-1 truncate">{view.name}</span>
            {view.shared ? (
              <Users aria-hidden className="size-3 shrink-0 text-muted-foreground" />
            ) : null}
          </button>
        );
      })}
    </section>
  );
}
