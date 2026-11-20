import { ArrowDown, ArrowUp, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import type { ChatEntry, ChatRole, Position } from "@/lib/prompts";

const ROLES: readonly ChatRole[] = ["system", "user", "assistant"];

type TemplateEditorProps = {
  /** Field errors by field path, such as `template` or `template[1].content`. */
  errors: Record<string, string>;
  /** The first source position of each field, appended to its message. */
  positions?: Record<string, Position>;
  disabled?: boolean;
} & (
  | { type: "text"; value: string; onChange: (value: string) => void }
  | { type: "chat"; value: ChatEntry[]; onChange: (value: ChatEntry[]) => void }
);

/** The message of a field, with its line and column when the server gave them. */
export function fieldMessage(
  field: string,
  errors: Record<string, string>,
  positions: Record<string, Position> | undefined,
): string | undefined {
  const message = errors[field];
  if (message === undefined) {
    return undefined;
  }
  const position = positions?.[field];
  return position ? `${message} (line ${position.line}, column ${position.column})` : message;
}

/** The text template or the chat messages of a prompt, with its field errors under each part. */
export function TemplateEditor(props: TemplateEditorProps) {
  if (props.type === "text") {
    return (
      <TextTemplate
        value={props.value}
        onChange={props.onChange}
        errors={props.errors}
        positions={props.positions}
        disabled={props.disabled}
      />
    );
  }
  return (
    <ChatTemplate
      value={props.value}
      onChange={props.onChange}
      errors={props.errors}
      positions={props.positions}
      disabled={props.disabled}
    />
  );
}

function TextTemplate({
  value,
  onChange,
  errors,
  positions,
  disabled,
}: {
  value: string;
  onChange: (value: string) => void;
  errors: Record<string, string>;
  positions: Record<string, Position> | undefined;
  disabled: boolean | undefined;
}) {
  const message = fieldMessage("template", errors, positions);
  return (
    <div className="flex flex-col gap-1.5">
      <Textarea
        aria-label="Template"
        className="font-mono text-body"
        spellCheck={false}
        rows={10}
        value={value}
        disabled={disabled}
        aria-invalid={message ? true : undefined}
        onChange={(event) => {
          onChange(event.target.value);
        }}
      />
      {message ? <p className="text-caption text-destructive">{message}</p> : null}
    </div>
  );
}

function ChatTemplate({
  value,
  onChange,
  errors,
  positions,
  disabled,
}: {
  value: ChatEntry[];
  onChange: (value: ChatEntry[]) => void;
  errors: Record<string, string>;
  positions: Record<string, Position> | undefined;
  disabled: boolean | undefined;
}) {
  const message = fieldMessage("template", errors, positions);

  function replaceAt(index: number, entry: ChatEntry) {
    onChange(value.map((item, position) => (position === index ? entry : item)));
  }

  function move(index: number, to: number) {
    const next = [...value];
    const [entry] = next.splice(index, 1);
    if (entry === undefined) {
      return;
    }
    next.splice(to, 0, entry);
    onChange(next);
  }

  return (
    <div className="flex flex-col gap-3">
      <ol className="flex flex-col gap-3">
        {value.map((entry, index) => {
          const number = index + 1;
          const noun = "type" in entry ? "placeholder" : "message";
          const entryMessage = [
            `template[${index}]`,
            `template[${index}].role`,
            `template[${index}].content`,
            `template[${index}].name`,
          ]
            .map((field) => fieldMessage(field, errors, positions))
            .find((text) => text !== undefined);
          const controls = (
            <div className="flex shrink-0 items-center gap-1">
              <Button
                type="button"
                variant="ghost"
                size="icon"
                aria-label={`Move ${noun} ${number} up`}
                disabled={disabled || index === 0}
                onClick={() => {
                  move(index, index - 1);
                }}
              >
                <ArrowUp aria-hidden="true" />
              </Button>
              <Button
                type="button"
                variant="ghost"
                size="icon"
                aria-label={`Move ${noun} ${number} down`}
                disabled={disabled || index === value.length - 1}
                onClick={() => {
                  move(index, index + 1);
                }}
              >
                <ArrowDown aria-hidden="true" />
              </Button>
              <Button
                type="button"
                variant="ghost"
                size="icon"
                aria-label={`Remove ${noun} ${number}`}
                disabled={disabled}
                onClick={() => {
                  onChange(value.filter((_, position) => position !== index));
                }}
              >
                <Trash2 aria-hidden="true" />
              </Button>
            </div>
          );
          if ("type" in entry) {
            return (
              <li
                // biome-ignore lint/suspicious/noArrayIndexKey: the rows are edited in place and have no id
                key={index}
                className="flex flex-col gap-1.5 rounded-md border p-3"
              >
                <div className="flex items-center gap-2">
                  <span className="text-caption text-muted-foreground">Placeholder</span>
                  <Input
                    aria-label={`Name of placeholder ${number}`}
                    className="h-7 w-48 text-body"
                    value={entry.name}
                    disabled={disabled}
                    aria-invalid={entryMessage ? true : undefined}
                    onChange={(event) => {
                      replaceAt(index, { type: "placeholder", name: event.target.value });
                    }}
                  />
                  <div className="ml-auto">{controls}</div>
                </div>
                {entryMessage ? (
                  <p className="text-caption text-destructive">{entryMessage}</p>
                ) : null}
              </li>
            );
          }
          return (
            <li
              // biome-ignore lint/suspicious/noArrayIndexKey: the rows are edited in place and have no id
              key={index}
              className="flex flex-col gap-1.5 rounded-md border p-3"
            >
              <div className="flex items-center gap-2">
                <Select
                  value={entry.role}
                  disabled={disabled}
                  onValueChange={(role) => {
                    replaceAt(index, { role: role as ChatRole, content: entry.content });
                  }}
                >
                  <SelectTrigger aria-label={`Role of message ${number}`} className="w-fit">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {ROLES.map((role) => (
                      <SelectItem key={role} value={role}>
                        {role}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <div className="ml-auto">{controls}</div>
              </div>
              <Textarea
                aria-label={`Content of message ${number}`}
                className="font-mono text-body"
                spellCheck={false}
                rows={3}
                value={entry.content}
                disabled={disabled}
                aria-invalid={entryMessage ? true : undefined}
                onChange={(event) => {
                  replaceAt(index, { role: entry.role, content: event.target.value });
                }}
              />
              {entryMessage ? (
                <p className="text-caption text-destructive">{entryMessage}</p>
              ) : null}
            </li>
          );
        })}
      </ol>
      <div className="flex flex-wrap gap-2">
        <Button
          type="button"
          variant="outline"
          size="sm"
          disabled={disabled}
          onClick={() => {
            onChange([...value, { role: "user", content: "" }]);
          }}
        >
          Add message
        </Button>
        <Button
          type="button"
          variant="outline"
          size="sm"
          disabled={disabled}
          onClick={() => {
            onChange([...value, { type: "placeholder", name: "history" }]);
          }}
        >
          Add placeholder
        </Button>
      </div>
      {message ? <p className="text-caption text-destructive">{message}</p> : null}
    </div>
  );
}
