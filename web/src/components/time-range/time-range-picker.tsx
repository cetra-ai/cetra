import { useParams } from "@tanstack/react-router";
import { Lock } from "lucide-react";
import { useState } from "react";
import { UpgradeButton } from "@/components/plan/upgrade-button";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Toggle } from "@/components/ui/toggle";
import { type TimeZoneSetting, useTimeZone } from "@/lib/settings";
import {
  beyondWindow,
  PRESETS,
  type Preset,
  parseTypedRange,
  presetLabel,
  type RangeState,
  rangeLabel,
  resolveRange,
} from "@/lib/time-range";

const TYPED_HINT = "Use 15m, 2h, 7d…";

export function TimeRangePicker({
  state,
  onChange,
}: {
  state: RangeState;
  onChange: (state: RangeState) => void;
}) {
  const zone = useTimeZone();
  const { org } = useParams({ from: "/$org" });
  const { query, upgradeUrl } = useOrgEntitlements(org);
  const windowDays = query.data?.limits.data_access_days;
  const [open, setOpen] = useState(false);
  const [typed, setTyped] = useState("");
  const [typedError, setTypedError] = useState(false);
  const [typedWindowError, setTypedWindowError] = useState(false);
  const [custom, setCustom] = useState(() => customSeed(state, zone));
  const [customError, setCustomError] = useState(false);
  const [customWindowError, setCustomWindowError] = useState(false);
  const lockedPresets = PRESETS.some((preset) =>
    beyondWindow({ kind: "preset", preset }, windowDays, Date.now()),
  );

  function apply(next: RangeState) {
    onChange(next);
    setOpen(false);
    setTyped("");
    setTypedError(false);
    setTypedWindowError(false);
    setCustomError(false);
    setCustomWindowError(false);
  }

  function applyTyped() {
    const parsed = parseTypedRange(typed);
    if (!parsed) {
      setTypedError(true);
      return;
    }
    if (typeof parsed === "string") {
      const next: RangeState = { kind: "preset", preset: parsed };
      if (beyondWindow(next, windowDays, Date.now())) {
        setTypedWindowError(true);
        return;
      }
      apply(next);
      return;
    }
    const now = Date.now();
    const next: RangeState = {
      kind: "absolute",
      from: new Date(now - parsed.durationMs).toISOString(),
      to: new Date(now).toISOString(),
    };
    if (beyondWindow(next, windowDays, now)) {
      setTypedWindowError(true);
      return;
    }
    apply(next);
  }

  function applyCustom() {
    const from = fromDateTimeLocal(custom.from, zone);
    const to = fromDateTimeLocal(custom.to, zone);
    if (!from || !to || Date.parse(from) >= Date.parse(to)) {
      setCustomError(true);
      return;
    }
    const next: RangeState = { kind: "absolute", from, to };
    if (beyondWindow(next, windowDays, Date.now())) {
      setCustomWindowError(true);
      return;
    }
    apply(next);
  }

  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (next) {
          setCustom(customSeed(state, zone));
          setCustomError(false);
          setCustomWindowError(false);
          setTypedError(false);
          setTypedWindowError(false);
        }
      }}
    >
      <PopoverTrigger asChild>
        <Button variant="outline" size="xs" className="font-normal">
          {rangeLabel(state, zone)}
        </Button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-64 gap-3 p-3">
        <div className="flex flex-col">
          {PRESETS.map((preset) => (
            <PresetRow
              key={preset}
              preset={preset}
              pressed={state.kind === "preset" && state.preset === preset}
              locked={beyondWindow({ kind: "preset", preset }, windowDays, Date.now())}
              onSelect={() => apply({ kind: "preset", preset })}
            />
          ))}
          {lockedPresets && windowDays !== undefined ? (
            <WindowNote windowDays={windowDays} upgradeUrl={upgradeUrl} />
          ) : null}
        </div>
        <form
          className="flex flex-col gap-1"
          onSubmit={(event) => {
            event.preventDefault();
            applyTyped();
          }}
        >
          <Label
            htmlFor="range-duration"
            className="text-caption font-normal text-muted-foreground"
          >
            Duration
          </Label>
          <Input
            id="range-duration"
            value={typed}
            placeholder="2h"
            aria-invalid={typedError}
            className="h-7 text-caption"
            onChange={(event) => {
              setTyped(event.target.value);
              setTypedError(false);
              setTypedWindowError(false);
            }}
          />
          {typedError ? <p className="text-caption text-destructive">{TYPED_HINT}</p> : null}
          {typedWindowError && windowDays !== undefined ? (
            <WindowNote windowDays={windowDays} upgradeUrl={upgradeUrl} error />
          ) : null}
        </form>
        <form
          className="flex flex-col gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            applyCustom();
          }}
        >
          <div className="flex flex-col gap-1">
            <Label htmlFor="range-from" className="text-caption font-normal text-muted-foreground">
              From
            </Label>
            <Input
              id="range-from"
              type="datetime-local"
              value={custom.from}
              aria-invalid={customError}
              className="h-7 text-caption"
              onChange={(event) => {
                setCustom((current) => ({ ...current, from: event.target.value }));
                setCustomError(false);
                setCustomWindowError(false);
              }}
            />
          </div>
          <div className="flex flex-col gap-1">
            <Label htmlFor="range-to" className="text-caption font-normal text-muted-foreground">
              To
            </Label>
            <Input
              id="range-to"
              type="datetime-local"
              value={custom.to}
              aria-invalid={customError}
              className="h-7 text-caption"
              onChange={(event) => {
                setCustom((current) => ({ ...current, to: event.target.value }));
                setCustomError(false);
                setCustomWindowError(false);
              }}
            />
          </div>
          {customError ? (
            <p className="text-caption text-destructive">From must be before to.</p>
          ) : null}
          {customWindowError && windowDays !== undefined ? (
            <WindowNote windowDays={windowDays} upgradeUrl={upgradeUrl} error />
          ) : null}
          <Button type="submit" size="xs" variant="secondary">
            Apply
          </Button>
        </form>
      </PopoverContent>
    </Popover>
  );
}

function PresetRow({
  preset,
  pressed,
  locked,
  onSelect,
}: {
  preset: Preset;
  pressed: boolean;
  locked: boolean;
  onSelect: () => void;
}) {
  const label = presetLabel(preset);
  return (
    <Toggle
      pressed={pressed}
      disabled={locked}
      size="sm"
      className="h-7 w-full justify-start px-2 text-caption font-normal"
      aria-label={locked ? `${label}, available on a paid plan` : undefined}
      onPressedChange={onSelect}
    >
      {locked ? <Lock className="size-3" aria-hidden="true" /> : null}
      {label}
    </Toggle>
  );
}

/** The data-window note: under the presets when some are locked, under a form it refused. */
function WindowNote({
  windowDays,
  upgradeUrl,
  error = false,
}: {
  windowDays: number;
  upgradeUrl: string | undefined;
  error?: boolean;
}) {
  const span = windowDays === 1 ? "1 day" : `${windowDays} days`;
  return (
    <p
      role="note"
      className={`flex flex-wrap items-center gap-1 text-caption ${error ? "text-destructive" : "text-muted-foreground"}`}
    >
      <Lock className="size-3" aria-hidden="true" />
      Data older than {span}: available on a paid plan.
      <UpgradeButton url={upgradeUrl} />
    </p>
  );
}

function customSeed(state: RangeState, zone: TimeZoneSetting): { from: string; to: string } {
  const resolved = state.kind === "absolute" ? state : resolveRange(state, Date.now());
  return {
    from: toDateTimeLocal(resolved.from, zone),
    to: toDateTimeLocal(resolved.to, zone),
  };
}

function toDateTimeLocal(rfc3339: string, zone: TimeZoneSetting): string {
  const date = new Date(rfc3339);
  if (Number.isNaN(date.getTime())) {
    return "";
  }
  const utc = zone === "utc";
  const year = utc ? date.getUTCFullYear() : date.getFullYear();
  const month = (utc ? date.getUTCMonth() : date.getMonth()) + 1;
  const day = utc ? date.getUTCDate() : date.getDate();
  const hours = utc ? date.getUTCHours() : date.getHours();
  const minutes = utc ? date.getUTCMinutes() : date.getMinutes();
  return `${year}-${pad(month)}-${pad(day)}T${pad(hours)}:${pad(minutes)}`;
}

function fromDateTimeLocal(value: string, zone: TimeZoneSetting): string | undefined {
  const match = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})$/.exec(value);
  if (!match) {
    return undefined;
  }
  const year = Number(match[1]);
  const month = Number(match[2]);
  const day = Number(match[3]);
  const hours = Number(match[4]);
  const minutes = Number(match[5]);
  const epoch =
    zone === "utc"
      ? Date.UTC(year, month - 1, day, hours, minutes)
      : new Date(year, month - 1, day, hours, minutes).getTime();
  if (Number.isNaN(epoch)) {
    return undefined;
  }
  return new Date(epoch).toISOString();
}

function pad(value: number): string {
  return String(value).padStart(2, "0");
}
