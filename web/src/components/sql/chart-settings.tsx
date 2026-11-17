import { useId } from "react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Label } from "@/components/ui/label";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Toggle } from "@/components/ui/toggle";
import { CHART_MAX_SERIES, type ChartConfig } from "@/lib/sql/chart";

/** Split-by value for "None". Radix Select rejects an empty value; no column is named with a NUL. */
const NONE = "\u0000none";

export type ChartColumns = { x: string[]; y: string[]; splitBy: string[] };

/**
 * Moves the x column. A y column equal to the new x is dropped (keeping at least one y), and a
 * split-by column equal to it is cleared.
 */
function withX(config: ChartConfig, columns: ChartColumns, x: string): ChartConfig {
  const kept = config.y.filter((name) => name !== x);
  const fallback = columns.y.find((name) => name !== x);
  return {
    ...config,
    x,
    y: kept.length > 0 || fallback === undefined ? kept : [fallback],
    splitBy: config.splitBy === x ? null : config.splitBy,
  };
}

/** One compact row of controls for the chart: type, x, y, split by. */
export function ChartSettings({
  config,
  columns,
  onChange,
}: {
  config: ChartConfig;
  columns: ChartColumns;
  onChange: (next: ChartConfig) => void;
}) {
  const yChoices = columns.y.filter((name) => name !== config.x);
  const splitChoices = columns.splitBy.filter((name) => name !== config.x);
  // Split by pivots only one y: with several checked, the chart ignores it.
  const splitEnabled = config.y.length === 1;
  const yCount = config.y.length;
  const id = useId();

  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5 text-body">
      <fieldset aria-label="Chart type" className="m-0 flex items-center gap-1 border-0 p-0">
        <span className="text-muted-foreground">Type</span>
        {(["line", "bar"] as const).map((kind) => (
          <Toggle
            key={kind}
            size="sm"
            variant="outline"
            pressed={config.kind === kind}
            onPressedChange={(pressed) => {
              if (pressed) {
                onChange({ ...config, kind });
              }
            }}
            className="h-7 px-2 text-body"
          >
            {kind === "line" ? "Line" : "Bar"}
          </Toggle>
        ))}
      </fieldset>

      <div className="flex items-center gap-1">
        <span className="text-muted-foreground">X</span>
        <Select value={config.x} onValueChange={(x) => onChange(withX(config, columns, x))}>
          <SelectTrigger size="sm" aria-label="X column" className="h-7 text-body">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {columns.x.map((name) => (
              <SelectItem key={name} value={name} className="text-body">
                {name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      <div className="flex items-center gap-1">
        <span className="text-muted-foreground">Y</span>
        <Popover>
          <PopoverTrigger asChild>
            <Button variant="outline" size="sm" aria-label="Y columns" className="h-7 text-body">
              {yCount} {yCount === 1 ? "column" : "columns"}
            </Button>
          </PopoverTrigger>
          <PopoverContent align="start" className="w-56 gap-2 p-2 text-body">
            {yChoices.map((name, index) => {
              const checked = config.y.includes(name);
              // At most CHART_MAX_SERIES columns, and at least one.
              const full = !checked && yCount >= CHART_MAX_SERIES;
              const last = checked && yCount === 1;
              const checkId = `${id}-y-${index}`;
              return (
                <div key={name} className="flex items-center gap-2">
                  <Checkbox
                    id={checkId}
                    checked={checked}
                    disabled={full || last}
                    onCheckedChange={(next) =>
                      onChange({
                        ...config,
                        y:
                          next === true
                            ? [...config.y, name]
                            : config.y.filter((other) => other !== name),
                      })
                    }
                  />
                  <Label htmlFor={checkId} className="truncate font-normal">
                    {name}
                  </Label>
                </div>
              );
            })}
          </PopoverContent>
        </Popover>
      </div>

      <div className="flex items-center gap-1">
        <span className="text-muted-foreground">Split by</span>
        <Select
          value={config.splitBy ?? NONE}
          disabled={!splitEnabled}
          onValueChange={(value) => onChange({ ...config, splitBy: value === NONE ? null : value })}
        >
          <SelectTrigger size="sm" aria-label="Split by column" className="h-7 text-body">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={NONE} className="text-body">
              None
            </SelectItem>
            {splitChoices.map((name) => (
              <SelectItem key={name} value={name} className="text-body">
                {name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
    </div>
  );
}
