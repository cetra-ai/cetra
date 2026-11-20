import { useQuery } from "@tanstack/react-query";
import { useFacetQuery } from "@/api/queries";
import { liveScoreDefinitionsQuery } from "@/api/queries-evaluation";
import { issueIdFromScoreName, issueScoreName, openIssuesQuery } from "@/api/queries-issues";
import type { FacetScope } from "@/components/facets/facet-panel";
import { useProject } from "@/components/shell/project-context";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Toggle } from "@/components/ui/toggle";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { appendFilter, type Chip, parseChip, serializeChip, splitConjuncts } from "@/lib/dsl/dsl";
import {
  type FacetView,
  PRESETS,
  type PresetDef,
  presetActive,
  roundP95,
  togglePreset,
} from "@/lib/facets";
import { formatCost, formatDuration } from "@/lib/format";

/** The preset chips of row 2 (D6). `scope` is the list's window and flags, as the facet panel's. */
export function PresetChips({
  view,
  filter,
  onFilter,
  scope,
}: {
  view: FacetView;
  filter: string;
  onFilter: (text: string) => void;
  scope: FacetScope;
}) {
  return (
    <div className="flex shrink-0 flex-wrap items-center gap-1">
      {PRESETS[view].map((preset) =>
        preset.p95Field === undefined ? (
          <FixedPreset key={preset.id} preset={preset} filter={filter} onFilter={onFilter} />
        ) : (
          <BoundPreset
            key={preset.id}
            preset={preset}
            p95Field={preset.p95Field}
            view={view}
            filter={filter}
            onFilter={onFilter}
            scope={scope}
          />
        ),
      )}
      {view === "traces" ? <HasScorePreset filter={filter} onFilter={onFilter} /> : null}
      {view === "traces" ? <HasIssuePreset filter={filter} onFilter={onFilter} /> : null}
    </div>
  );
}

/** Errors and the spans' LLM preset: a fixed conjunct. */
function FixedPreset({
  preset,
  filter,
  onFilter,
}: {
  preset: PresetDef;
  filter: string;
  onFilter: (text: string) => void;
}) {
  return (
    <Toggle
      size="sm"
      className="h-6 px-2"
      pressed={presetActive(filter, preset)}
      onPressedChange={() => onFilter(togglePreset(filter, preset))}
    >
      {preset.label}
    </Toggle>
  );
}

/** Slow and Expensive: a lower bound at the window's p95 of the field (D6). */
function BoundPreset({
  preset,
  p95Field,
  view,
  filter,
  onFilter,
  scope,
}: {
  preset: PresetDef;
  p95Field: string;
  view: FacetView;
  filter: string;
  onFilter: (text: string) => void;
  scope: FacetScope;
}) {
  const { org, project } = useProject();
  const query = useFacetQuery(org, project, {
    view,
    field: p95Field,
    from: scope.from,
    to: scope.to,
    includeEval: scope.includeEval,
    includeExperiments: scope.includeExperiments,
    limit: 1,
  });
  const p95 = query.data?.histogram?.p95;
  const bound = p95 === undefined ? undefined : roundP95(p95);
  const pressed = presetActive(filter, preset);
  // Turning an active preset off needs no p95, so only an inactive one waits for it.
  const disabled = !pressed && bound === undefined;
  let tip: string;
  if (bound !== undefined) {
    tip = p95Tip(preset, bound);
  } else if (query.isPending) {
    tip = "Loading the p95 of this window";
  } else {
    tip = "No p95: the window has no values";
  }
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="inline-flex">
          <Toggle
            size="sm"
            className="h-6 px-2"
            pressed={pressed}
            disabled={disabled}
            onPressedChange={() => onFilter(togglePreset(filter, preset, bound))}
          >
            {preset.label}
          </Toggle>
        </span>
      </TooltipTrigger>
      <TooltipContent>{tip}</TooltipContent>
    </Tooltip>
  );
}

function p95Tip(preset: PresetDef, bound: number): string {
  if (preset.id === "slow") {
    // duration_ms is in milliseconds; formatDuration takes nanoseconds.
    return `Slower than p95 (${formatDuration(bound * 1_000_000)})`;
  }
  return `Costlier than p95 (${formatCost(bound)})`;
}

/** The names of the `has_score` conjuncts the filter holds (not negated). */
function scoreNamesIn(filter: string): Set<string> {
  const names = new Set<string>();
  for (const conjunct of splitConjuncts(filter)) {
    const chip = parseChip(conjunct.text);
    if (chip.kind === "has_score" && !chip.negated) {
      names.add(chip.name);
    }
  }
  return names;
}

/** The filter with `has_score('name')` added, or removed when it is already there (D6). */
function toggleHasScore(filter: string, name: string): string {
  const isThis = (text: string): boolean => {
    const chip: Chip = parseChip(text);
    return chip.kind === "has_score" && !chip.negated && chip.name === name;
  };
  const conjuncts = splitConjuncts(filter);
  if (!conjuncts.some((conjunct) => isThis(conjunct.text))) {
    return appendFilter(filter, serializeChip({ kind: "has_score", name, negated: false }));
  }
  return conjuncts
    .filter((conjunct) => !isThis(conjunct.text))
    .reduce((joined, conjunct) => appendFilter(joined, conjunct.text), "");
}

/** The score definitions' names in the filter: issue names (D-IC-18) are not score definitions. */
function hasScoreNames(active: Set<string>): string[] {
  return [...active].filter((name) => issueIdFromScoreName(name) === undefined);
}

/** Has score: a menu of the project's live score definitions, one checked item per name (D6). */
function HasScorePreset({
  filter,
  onFilter,
}: {
  filter: string;
  onFilter: (text: string) => void;
}) {
  const { org, project } = useProject();
  const definitions = useQuery(liveScoreDefinitionsQuery(org, project));
  const active = scoreNamesIn(filter);
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Toggle size="sm" className="h-6 px-2" pressed={hasScoreNames(active).length > 0}>
          Has score
        </Toggle>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start">
        {definitions.data?.data.length === 0 ? (
          <DropdownMenuLabel>No score definitions</DropdownMenuLabel>
        ) : null}
        {definitions.data?.data.map((definition) => (
          <DropdownMenuCheckboxItem
            key={definition.id}
            checked={active.has(definition.name)}
            onCheckedChange={() => onFilter(toggleHasScore(filter, definition.name))}
          >
            {definition.name}
          </DropdownMenuCheckboxItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

/** Has issue: a menu of the 20 most recent open issues, one checked item per issue (track IC). */
function HasIssuePreset({
  filter,
  onFilter,
}: {
  filter: string;
  onFilter: (text: string) => void;
}) {
  const { org, project } = useProject();
  const issues = useQuery(openIssuesQuery(org, project));
  const active = scoreNamesIn(filter);
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Toggle
          size="sm"
          className="h-6 px-2"
          pressed={[...active].some((name) => issueIdFromScoreName(name) !== undefined)}
        >
          Has issue
        </Toggle>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start">
        {issues.data?.data.length === 0 ? (
          <DropdownMenuLabel>No open issues</DropdownMenuLabel>
        ) : null}
        {issues.data?.data.map((issue) => (
          <DropdownMenuCheckboxItem
            key={issue.id}
            checked={active.has(issueScoreName(issue.id))}
            onCheckedChange={() => onFilter(toggleHasScore(filter, issueScoreName(issue.id)))}
          >
            {issue.title}
          </DropdownMenuCheckboxItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
