import { RefreshCw } from "lucide-react";
import { type ReactNode, type RefObject, useEffect, useRef, useState } from "react";
import { useShortcut } from "@/components/command/use-shortcut";
import { TimeRangePicker } from "@/components/time-range/time-range-picker";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import type { RangeState } from "@/lib/time-range";

const SEARCH_DEBOUNCE_MS = 300;

export function ListToolbar({
  title,
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
  columns,
  views,
  loaded,
  live,
  updatedAt,
}: {
  title: string;
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
  filter?: ReactNode;
  columns?: ReactNode;
  views?: ReactNode;
  loaded?: number;
  live?: ReactNode;
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
    <div
      ref={barRef}
      className="flex min-h-9 shrink-0 items-center gap-2 overflow-x-auto border-b px-3"
    >
      <h1 className="shrink-0 text-body font-medium">{title}</h1>
      <TimeRangePicker state={range} onChange={onRange} />
      <Button type="button" variant="ghost" size="icon-xs" aria-label="Refresh" onClick={onRefresh}>
        <RefreshCw />
      </Button>
      {updatedAt !== undefined ? <UpdatedAgo at={updatedAt} /> : null}
      {live}
      <div className="min-w-0 flex-1">{filter}</div>
      {onSearch ? <SearchBox inputRef={searchRef} value={search ?? ""} onValue={onSearch} /> : null}
      <Flag
        id={`${idPrefix}-eval`}
        label="Eval"
        tooltip="Show evaluation traces"
        checked={evalOn}
        onCheckedChange={onEval}
      />
      <Flag
        id={`${idPrefix}-exp`}
        label="Experiments"
        tooltip="Show experiment traces"
        checked={expOn}
        onCheckedChange={onExp}
      />
      {columns}
      {views}
      {loaded !== undefined ? (
        <span className="shrink-0 text-caption text-muted-foreground">{loaded} loaded</span>
      ) : null}
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

function Flag({
  id,
  label,
  tooltip,
  checked,
  onCheckedChange,
}: {
  id: string;
  label: string;
  tooltip: string;
  checked: boolean;
  onCheckedChange: (on: boolean) => void;
}) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="inline-flex shrink-0 items-center gap-1.5">
          <Switch id={id} size="sm" checked={checked} onCheckedChange={onCheckedChange} />
          <Label htmlFor={id} className="text-caption font-normal">
            {label}
          </Label>
        </span>
      </TooltipTrigger>
      <TooltipContent>{tooltip}</TooltipContent>
    </Tooltip>
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
