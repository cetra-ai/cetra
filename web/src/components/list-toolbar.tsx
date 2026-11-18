import { RefreshCw } from "lucide-react";
import { type ReactNode, type RefObject, useEffect, useRef, useState } from "react";
import { useShortcut } from "@/components/command/use-shortcut";
import { TimeRangePicker } from "@/components/time-range/time-range-picker";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import type { RangeState } from "@/lib/time-range";

const SEARCH_DEBOUNCE_MS = 300;

export function ListToolbar({
  range,
  onRange,
  onRefresh,
  search,
  onSearch,
  idPrefix = "traces",
  evalOn,
  onEval,
  expOn,
  onExp,
  filter,
  filtersToggle,
  presets,
  columns,
  views,
  loaded,
  live,
  notice,
  updatedAt,
}: {
  range: RangeState;
  onRange: (state: RangeState) => void;
  onRefresh: () => void;
  search?: string;
  onSearch?: (value: string) => void;
  idPrefix?: string;
  evalOn: boolean;
  onEval: (on: boolean) => void;
  expOn: boolean;
  onExp: (on: boolean) => void;
  /** Row 2: the filter bar, shown whenever `filter` is set. */
  filter?: ReactNode;
  /** Row 2, before the presets: the facet panel toggle (D2). */
  filtersToggle?: ReactNode;
  /** Row 2, after the toggle: the preset chips (D6). */
  presets?: ReactNode;
  columns?: ReactNode;
  views?: ReactNode;
  loaded?: number;
  live?: ReactNode;
  /** Below row 2: a notice about the list's filters, e.g. the filters carried in (D8). */
  notice?: ReactNode;
  /** Epoch milliseconds of the data on screen. Shown as "Updated N ago" beside refresh when set. */
  updatedAt?: number;
}) {
  const searchRef = useRef<HTMLInputElement>(null);
  const barRef = useRef<HTMLDivElement>(null);

  useShortcut("focus-search", (event) => {
    if (onSearch) {
      event.preventDefault();
      searchRef.current?.focus();
      return;
    }
    const filter =
      barRef.current?.querySelector<HTMLElement>("[aria-label='Edit filter']") ??
      barRef.current?.querySelector<HTMLElement>(".cm-content");
    if (!filter) {
      return;
    }
    event.preventDefault();
    filter.focus();
  });

  return (
    <div ref={barRef} className="shrink-0">
      <div className="flex min-h-9 items-center gap-2 overflow-x-auto border-b px-3">
        <TimeRangePicker state={range} onChange={onRange} />
        <Button
          type="button"
          variant="ghost"
          size="icon-xs"
          aria-label="Refresh"
          onClick={onRefresh}
        >
          <RefreshCw />
        </Button>
        {updatedAt !== undefined ? <UpdatedAgo at={updatedAt} /> : null}
        {live}
        <div className="min-w-0 flex-1" />
        {onSearch ? (
          <SearchBox inputRef={searchRef} value={search ?? ""} onValue={onSearch} />
        ) : null}
        <IncludeMenu
          id={`${idPrefix}-include`}
          evalOn={evalOn}
          onEval={onEval}
          expOn={expOn}
          onExp={onExp}
        />
        {columns}
        {views}
        {loaded !== undefined ? (
          <span className="shrink-0 text-caption text-muted-foreground">{loaded} loaded</span>
        ) : null}
      </div>
      {filter ? (
        <div className="flex min-h-9 flex-wrap items-center gap-2 border-b px-3 py-1">
          {filtersToggle}
          {presets}
          <div className="min-w-48 flex-1 basis-64">{filter}</div>
        </div>
      ) : null}
      {notice}
    </div>
  );
}

const UPDATED_TICK_MS = 10_000;

function UpdatedAgo({ at }: { at: number }) {
  const [, setTick] = useState(0);
  useEffect(() => {
    const timer = window.setInterval(() => setTick((tick) => tick + 1), UPDATED_TICK_MS);
    return () => window.clearInterval(timer);
  }, []);
  const seconds = Math.max(0, Math.floor((Date.now() - at) / 1000));
  const label =
    seconds < 60 ? `Updated ${seconds}s ago` : `Updated ${Math.floor(seconds / 60)} min ago`;
  return <span className="shrink-0 text-caption text-muted-foreground">{label}</span>;
}

function IncludeMenu({
  id,
  evalOn,
  onEval,
  expOn,
  onExp,
}: {
  id: string;
  evalOn: boolean;
  onEval: (on: boolean) => void;
  expOn: boolean;
  onExp: (on: boolean) => void;
}) {
  const count = Number(evalOn) + Number(expOn);
  const name = "Include evaluation and experiment traces";
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          id={id}
          type="button"
          variant="ghost"
          size="xs"
          aria-label={count > 0 ? `${name}, ${count} on` : name}
          className="shrink-0"
        >
          {count > 0 ? `Include · ${count}` : "Include"}
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <DropdownMenuCheckboxItem
          checked={evalOn}
          onCheckedChange={onEval}
          onSelect={(event) => event.preventDefault()}
        >
          Evaluation traces
        </DropdownMenuCheckboxItem>
        <DropdownMenuCheckboxItem
          checked={expOn}
          onCheckedChange={onExp}
          onSelect={(event) => event.preventDefault()}
        >
          Experiment traces
        </DropdownMenuCheckboxItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function SearchBox({
  inputRef,
  value,
  onValue,
}: {
  inputRef: RefObject<HTMLInputElement | null>;
  value: string;
  onValue: (value: string) => void;
}) {
  const [draft, setDraft] = useState(value);
  const applied = useRef(value);
  const timer = useRef<number | null>(null);

  useEffect(() => {
    if (value !== applied.current) {
      applied.current = value;
      setDraft(value);
    }
  }, [value]);

  useEffect(() => {
    return () => {
      if (timer.current !== null) {
        window.clearTimeout(timer.current);
      }
    };
  }, []);

  function publish(next: string) {
    if (timer.current !== null) {
      window.clearTimeout(timer.current);
      timer.current = null;
    }
    applied.current = next.trim();
    onValue(next);
  }

  function schedule(next: string) {
    if (timer.current !== null) {
      window.clearTimeout(timer.current);
    }
    timer.current = window.setTimeout(() => {
      timer.current = null;
      applied.current = next.trim();
      onValue(next);
    }, SEARCH_DEBOUNCE_MS);
  }

  return (
    <Input
      ref={inputRef}
      type="search"
      aria-label="Search"
      value={draft}
      placeholder="Search"
      className="h-7 w-44 shrink-0 text-caption"
      onChange={(event) => {
        setDraft(event.target.value);
        schedule(event.target.value);
      }}
      onKeyDown={(event) => {
        if (event.key === "Enter") {
          event.preventDefault();
          publish(draft);
        }
        if (event.key === "Escape") {
          event.preventDefault();
          setDraft("");
          publish("");
        }
      }}
    />
  );
}
