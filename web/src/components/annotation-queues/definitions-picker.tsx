import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { ArrowDown, ArrowUp } from "lucide-react";
import { type ReactNode, useId } from "react";
import { liveScoreDefinitionsQuery } from "@/api/queries-evaluation";
import type { Schema } from "@/api/types";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";

type ScoreDefinition = Schema<"ScoreDefinition">;

/** A queue holds at most this many score definitions. */
export const MAX_QUEUE_DEFINITIONS = 50;

/**
 * The live score definitions of a queue, as checkboxes. The checked ones come first, in `value`'s
 * order, with buttons to move them up and down; the others follow, sorted by name. `value` is the
 * queue's `definition_ids` in the order the reviewer sees them.
 */
export function DefinitionsPicker({
  value,
  onChange,
}: {
  value: readonly string[];
  onChange: (next: string[]) => void;
}) {
  const { org, project } = useProject();
  const definitions = useQuery(liveScoreDefinitionsQuery(org, project));
  const id = useId();

  if (definitions.isPending) {
    return <Skeleton className="h-4 w-2/3" />;
  }
  if (definitions.isError) {
    return <p className="text-caption text-destructive">Could not load the score definitions.</p>;
  }
  const live = definitions.data.data;
  if (live.length === 0) {
    return (
      <div className="flex flex-col gap-1">
        <p className="text-caption text-muted-foreground">No score definitions yet.</p>
        <Link
          to="/$org/$project/settings/score-definitions"
          params={{ org, project }}
          className="w-fit text-caption underline underline-offset-4"
        >
          Add score definitions
        </Link>
      </div>
    );
  }

  const byId = new Map(live.map((definition) => [definition.id, definition]));
  const checked = value.flatMap((definitionId) => {
    const definition = byId.get(definitionId);
    return definition === undefined ? [] : [definition];
  });
  const unchecked = live
    .filter((definition) => !value.includes(definition.id))
    .sort((left, right) => left.name.localeCompare(right.name));
  const full = value.length >= MAX_QUEUE_DEFINITIONS;

  // Swaps two checked definitions by id. `value` may hold archived ids the picker does not show,
  // so positions are looked up in `value` rather than taken from `checked`.
  function swap(first: string, second: string) {
    const next = [...value];
    const firstAt = next.indexOf(first);
    const secondAt = next.indexOf(second);
    next[firstAt] = second;
    next[secondAt] = first;
    onChange(next);
  }

  return (
    <div className="flex flex-col gap-2">
      <ul className="flex flex-col gap-1.5">
        {checked.map((definition, index) => (
          <DefinitionItem
            key={definition.id}
            id={`${id}-${definition.id}`}
            definition={definition}
            checked
            disabled={false}
            onCheckedChange={() => {
              onChange(value.filter((item) => item !== definition.id));
            }}
            controls={
              <>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-xs"
                  aria-label={`Move ${definition.name} up`}
                  disabled={index === 0}
                  onClick={() => {
                    const above = checked[index - 1];
                    if (above) {
                      swap(definition.id, above.id);
                    }
                  }}
                >
                  <ArrowUp aria-hidden="true" />
                </Button>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-xs"
                  aria-label={`Move ${definition.name} down`}
                  disabled={index === checked.length - 1}
                  onClick={() => {
                    const below = checked[index + 1];
                    if (below) {
                      swap(definition.id, below.id);
                    }
                  }}
                >
                  <ArrowDown aria-hidden="true" />
                </Button>
              </>
            }
          />
        ))}
        {unchecked.map((definition) => (
          <DefinitionItem
            key={definition.id}
            id={`${id}-${definition.id}`}
            definition={definition}
            checked={false}
            disabled={full}
            onCheckedChange={() => {
              onChange([...value, definition.id]);
            }}
          />
        ))}
      </ul>
      {full ? (
        <p className="text-caption text-muted-foreground">
          A queue holds at most {MAX_QUEUE_DEFINITIONS} definitions.
        </p>
      ) : null}
    </div>
  );
}

function DefinitionItem({
  id,
  definition,
  checked,
  disabled,
  onCheckedChange,
  controls,
}: {
  id: string;
  definition: ScoreDefinition;
  checked: boolean;
  disabled: boolean;
  onCheckedChange: () => void;
  controls?: ReactNode;
}) {
  return (
    <li className="flex items-center gap-2">
      <Checkbox id={id} checked={checked} disabled={disabled} onCheckedChange={onCheckedChange} />
      <Label htmlFor={id} className="min-w-0 truncate">
        {definition.name}
      </Label>
      <span className="text-caption text-muted-foreground">{definition.data_type}</span>
      {controls ? <div className="ml-auto flex items-center gap-0.5">{controls}</div> : null}
    </li>
  );
}
