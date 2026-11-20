import { ChevronRight } from "lucide-react";
import { memo, type ReactNode, useState } from "react";
import { placeholderNamesOf } from "@/components/playground/message-editor";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { parseJsonField } from "@/lib/datasets";
import { type Draft, draftVariables, storedText } from "@/lib/playground";

/** The choices of `tool_choice` besides a tool name. */
const TOOL_CHOICES = ["auto", "none", "required"] as const;
/** The select's value for an unset tool choice (an empty string is not a select value). */
const NO_CHOICE = "__unset__";

type SettingsPanelProps = {
  draft: Draft;
  onChange: (patch: Partial<Draft>) => void;
  /** Field errors of the settings, by field name (`tools`, `placeholders.<name>`, ...). */
  errors: Record<string, string>;
};

/** The tool names of a JSON list of tools; `[]` when the text is not one. */
function toolNamesOf(text: string): string[] {
  const parsed = parseJsonField(text, { required: false });
  if (!parsed.ok || !Array.isArray(parsed.value)) {
    return [];
  }
  const names: string[] = [];
  for (const item of parsed.value as unknown[]) {
    const name =
      typeof item === "object" && item !== null ? (item as { name?: unknown }).name : undefined;
    if (typeof name === "string" && !names.includes(name)) {
      names.push(name);
    }
  }
  return names;
}

/** The variables, placeholders, tools and response format of a draft. */
export const SettingsPanel = memo(function SettingsPanel({
  draft,
  onChange,
  errors,
}: SettingsPanelProps) {
  const variables = draftVariables(draft);
  const placeholders = placeholderNamesOf(draft.entries);
  const toolNames = toolNamesOf(draft.tools).filter(
    (name) => !(TOOL_CHOICES as readonly string[]).includes(name),
  );
  return (
    <div className="flex flex-col gap-6">
      <div className="flex flex-col gap-2">
        <div className="flex flex-wrap items-center gap-2">
          <h2 className="text-body font-medium">Variables</h2>
          <div className="ml-auto flex items-center gap-2 text-body">
            <Switch
              id="playground-render-variables"
              checked={draft.renderVariables}
              onCheckedChange={(checked) => {
                onChange({ renderVariables: checked });
              }}
            />
            <Label htmlFor="playground-render-variables">Treat {"{{ }}"} as variables</Label>
          </div>
        </div>
        {variables.map((name) => (
          <div key={name} className="flex flex-col gap-1">
            <span className="text-caption text-muted-foreground">{name}</span>
            <Input
              aria-label={name}
              className="h-7 text-body"
              value={storedText(draft.variables, name)}
              aria-invalid={errors[`variables.${name}`] ? true : undefined}
              onChange={(event) => {
                onChange({ variables: { ...draft.variables, [name]: event.target.value } });
              }}
            />
            {errors[`variables.${name}`] ? (
              <p className="text-caption text-destructive">{errors[`variables.${name}`]}</p>
            ) : null}
          </div>
        ))}
      </div>

      {placeholders.length === 0 ? null : (
        <div className="flex flex-col gap-2">
          <h2 className="text-body font-medium">Placeholders</h2>
          {placeholders.map((name) => (
            <div key={name} className="flex flex-col gap-1">
              <span className="text-caption text-muted-foreground">{name}</span>
              <Textarea
                aria-label={`Placeholder ${name}`}
                className="font-mono text-body"
                spellCheck={false}
                rows={3}
                placeholder="A JSON list of messages"
                value={storedText(draft.placeholders, name)}
                aria-invalid={errors[`placeholders.${name}`] ? true : undefined}
                onChange={(event) => {
                  onChange({ placeholders: { ...draft.placeholders, [name]: event.target.value } });
                }}
              />
              {errors[`placeholders.${name}`] ? (
                <p className="text-caption text-destructive">{errors[`placeholders.${name}`]}</p>
              ) : null}
            </div>
          ))}
        </div>
      )}

      <Section
        title="Tools"
        badge={toolsBadge(draft.tools)}
        filled={draft.tools.trim() !== "" || draft.toolChoice !== ""}
        invalid={Boolean(errors.tools ?? errors.tool_choice ?? errors.toolChoice)}
      >
        <Textarea
          aria-label="Tools"
          className="font-mono text-body"
          spellCheck={false}
          rows={5}
          placeholder='[{"name": "lookup", "parameters": {"type": "object"}}]'
          value={draft.tools}
          aria-invalid={errors.tools ? true : undefined}
          onChange={(event) => {
            onChange({ tools: event.target.value });
          }}
        />
        {errors.tools ? <p className="text-caption text-destructive">{errors.tools}</p> : null}
        <div className="flex items-center gap-2">
          <Label htmlFor="playground-tool-choice" className="text-muted-foreground">
            Tool choice
          </Label>
          <Select
            value={draft.toolChoice === "" ? NO_CHOICE : draft.toolChoice}
            onValueChange={(value) => {
              onChange({ toolChoice: value === NO_CHOICE ? "" : value });
            }}
          >
            <SelectTrigger id="playground-tool-choice" className="w-fit">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={NO_CHOICE}>Not set</SelectItem>
              {TOOL_CHOICES.map((choice) => (
                <SelectItem key={choice} value={choice}>
                  {choice}
                </SelectItem>
              ))}
              {toolNames.map((name) => (
                <SelectItem key={`tool:${name}`} value={name}>
                  {name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        {(errors.tool_choice ?? errors.toolChoice) ? (
          <p className="text-caption text-destructive">{errors.tool_choice ?? errors.toolChoice}</p>
        ) : null}
      </Section>

      <Section
        title="Response format"
        badge={draft.responseFormat.trim() === "" ? undefined : "set"}
        filled={draft.responseFormat.trim() !== ""}
        invalid={Boolean(errors.responseFormat ?? errors.response_format)}
      >
        <Textarea
          aria-label="Response format"
          className="font-mono text-body"
          spellCheck={false}
          rows={5}
          placeholder='{"name": "answer", "schema": {"type": "object"}}'
          value={draft.responseFormat}
          aria-invalid={(errors.responseFormat ?? errors.response_format) ? true : undefined}
          onChange={(event) => {
            onChange({ responseFormat: event.target.value });
          }}
        />
        {(errors.responseFormat ?? errors.response_format) ? (
          <p className="text-caption text-destructive">
            {errors.responseFormat ?? errors.response_format}
          </p>
        ) : null}
      </Section>
    </div>
  );
});

/** The Tools section's badge: the number of tools, "set" when the text is not a list yet. */
function toolsBadge(text: string): string | undefined {
  if (text.trim() === "") {
    return undefined;
  }
  const count = toolNamesOf(text).length;
  return count === 0 ? "set" : String(count);
}

/**
 * A section that folds under its heading. It starts open when it holds something or an error, and
 * stays as the user leaves it once toggled.
 */
function Section({
  title,
  badge,
  filled,
  invalid,
  children,
}: {
  title: string;
  badge: string | undefined;
  filled: boolean;
  invalid: boolean;
  children: ReactNode;
}) {
  const [toggled, setToggled] = useState<boolean | undefined>(undefined);
  const open = invalid || (toggled ?? filled);
  return (
    <section className="flex flex-col gap-2">
      <h2 className="text-body font-medium">
        <button
          type="button"
          aria-expanded={open}
          className="flex items-center gap-1.5 rounded-sm outline-none focus-visible:ring-3 focus-visible:ring-ring/50"
          onClick={() => {
            setToggled(!open);
          }}
        >
          <ChevronRight
            aria-hidden="true"
            className={`size-3.5 text-muted-foreground transition-transform ${open ? "rotate-90" : ""}`}
          />
          {title}
          {badge === undefined ? null : (
            <Badge variant="secondary" className={invalid ? "text-destructive" : undefined}>
              {badge}
            </Badge>
          )}
        </button>
      </h2>
      {open ? children : null}
    </section>
  );
}
