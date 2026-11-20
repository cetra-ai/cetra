import { placeholderNamesOf } from "@/components/playground/message-editor";
import { Input } from "@/components/ui/input";
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
import { type Draft, draftVariables } from "@/lib/playground";

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
export function SettingsPanel({ draft, onChange, errors }: SettingsPanelProps) {
  const variables = draftVariables(draft);
  const placeholders = placeholderNamesOf(draft.entries);
  const toolNames = toolNamesOf(draft.tools).filter(
    (name) => !(TOOL_CHOICES as readonly string[]).includes(name),
  );
  return (
    <div className="flex flex-col gap-6">
      <div className="flex flex-col gap-2">
        <h2 className="text-body font-medium">Variables</h2>
        <label className="flex items-center gap-2 text-body">
          <Switch
            checked={draft.renderVariables}
            onCheckedChange={(checked) => {
              onChange({ renderVariables: checked });
            }}
          />
          Treat {"{{ }}"} as variables
        </label>
        {variables.length === 0 ? (
          <p className="text-caption text-muted-foreground">No variables.</p>
        ) : null}
        {variables.map((name) => (
          <div key={name} className="flex flex-col gap-1">
            <span className="text-caption text-muted-foreground">{name}</span>
            <Input
              aria-label={name}
              className="h-7 text-body"
              value={draft.variables[name] ?? ""}
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

      <div className="flex flex-col gap-2">
        <h2 className="text-body font-medium">Placeholders</h2>
        {placeholders.length === 0 ? (
          <p className="text-caption text-muted-foreground">No placeholders.</p>
        ) : null}
        {placeholders.map((name) => (
          <div key={name} className="flex flex-col gap-1">
            <span className="text-caption text-muted-foreground">{name}</span>
            <Textarea
              aria-label={`Placeholder ${name}`}
              className="font-mono text-body"
              spellCheck={false}
              rows={3}
              placeholder="A JSON list of messages"
              value={draft.placeholders[name] ?? ""}
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

      <div className="flex flex-col gap-2">
        <h2 className="text-body font-medium">Tools</h2>
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
        <Select
          value={draft.toolChoice === "" ? NO_CHOICE : draft.toolChoice}
          onValueChange={(value) => {
            onChange({ toolChoice: value === NO_CHOICE ? "" : value });
          }}
        >
          <SelectTrigger aria-label="Tool choice" className="w-fit">
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
        {errors.tool_choice ?? errors.toolChoice ? (
          <p className="text-caption text-destructive">{errors.tool_choice ?? errors.toolChoice}</p>
        ) : null}
      </div>

      <div className="flex flex-col gap-2">
        <h2 className="text-body font-medium">Response format</h2>
        <Textarea
          aria-label="Response format"
          className="font-mono text-body"
          spellCheck={false}
          rows={5}
          placeholder='{"name": "answer", "schema": {"type": "object"}}'
          value={draft.responseFormat}
          aria-invalid={errors.responseFormat ?? errors.response_format ? true : undefined}
          onChange={(event) => {
            onChange({ responseFormat: event.target.value });
          }}
        />
        {errors.responseFormat ?? errors.response_format ? (
          <p className="text-caption text-destructive">
            {errors.responseFormat ?? errors.response_format}
          </p>
        ) : null}
      </div>
    </div>
  );
}
