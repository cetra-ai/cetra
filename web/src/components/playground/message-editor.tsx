import { ArrowDown, ArrowUp, Trash2 } from "lucide-react";
import { memo, useRef, useState } from "react";
import { Badge } from "@/components/ui/badge";
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
import { parseJsonField, prettyJson } from "@/lib/datasets";
import type { Entry, Message, Part, Role } from "@/lib/playground";
import { uuidv7 } from "@/lib/uuid";

const ROLES: readonly Role[] = ["system", "user", "assistant", "tool"];

type MessageEditorProps = {
  entries: Entry[];
  /** The new entries. `rename` is given when a placeholder's name changed, so its text moves too. */
  onChange: (entries: Entry[], rename?: { from: string; to: string }) => void;
  /** Problems by entry position (0-based): the refused parts, or a server field error. */
  errors: Record<number, string>;
};

export function isPlaceholder(entry: Entry): entry is { type: "placeholder"; name: string } {
  return "type" in entry && entry.type === "placeholder";
}

/** The placeholder names of the entries, in order of first use. */
export function placeholderNamesOf(entries: readonly Entry[]): string[] {
  const names: string[] = [];
  for (const entry of entries) {
    if (isPlaceholder(entry) && !names.includes(entry.name)) {
      names.push(entry.name);
    }
  }
  return names;
}

function isTextPart(part: Part): part is { type: "text"; content: string } {
  return part.type === "text" && "content" in part && typeof part.content === "string";
}

/** The text parts of a message joined by a newline. */
function textOfParts(parts: Part[]): string {
  return parts.flatMap((part) => (isTextPart(part) ? [part.content] : [])).join("\n");
}

/** A tool message, or one with non-text parts, is edited as its parts' JSON. */
function editsAsParts(message: Message): boolean {
  return message.role === "tool" || !message.parts.every(isTextPart);
}

/** The parts of a message as JSON text. Invalid text is kept while it is typed. */
function PartsEditor({
  number,
  parts,
  onChange,
}: {
  number: number;
  parts: Part[];
  onChange: (parts: Part[]) => void;
}) {
  const [text, setText] = useState(() => prettyJson(parts));
  const [problem, setProblem] = useState<string | undefined>(undefined);
  return (
    <div className="flex flex-col gap-1.5">
      <Textarea
        aria-label={`Parts of message ${number}`}
        className="font-mono text-body"
        spellCheck={false}
        rows={6}
        value={text}
        aria-invalid={problem ? true : undefined}
        onChange={(event) => {
          const next = event.target.value;
          setText(next);
          const parsed = parseJsonField(next, { required: true });
          if (!parsed.ok) {
            setProblem(parsed.message);
            return;
          }
          const value = parsed.value;
          if (
            !Array.isArray(value) ||
            !value.every(
              (part) =>
                typeof part === "object" &&
                part !== null &&
                !Array.isArray(part) &&
                typeof (part as { type?: unknown }).type === "string",
            )
          ) {
            setProblem("Must be a JSON list of parts");
            return;
          }
          setProblem(undefined);
          onChange(value as Part[]);
        }}
      />
      {problem ? <p className="text-caption text-destructive">{problem}</p> : null}
    </div>
  );
}

/** The ordered messages and placeholders of a draft. Message numbers count every entry. */
export const MessageEditor = memo(function MessageEditor({
  entries,
  onChange,
  errors,
}: MessageEditorProps) {
  // React-only row ids, one per entry. A row keeps its id while its neighbours move or go, so
  // its local state (a parts editor's text) stays with its own entry.
  const rowIds = useRef<string[]>([]);
  while (rowIds.current.length < entries.length) {
    rowIds.current.push(uuidv7());
  }
  rowIds.current.length = entries.length;

  function replaceAt(index: number, entry: Entry, rename?: { from: string; to: string }) {
    onChange(
      entries.map((item, position) => (position === index ? entry : item)),
      rename,
    );
  }

  function move(index: number, to: number) {
    const next = [...entries];
    const ids = [...rowIds.current];
    const [entry] = next.splice(index, 1);
    const [id] = ids.splice(index, 1);
    if (entry === undefined || id === undefined) {
      return;
    }
    next.splice(to, 0, entry);
    ids.splice(to, 0, id);
    rowIds.current = ids;
    onChange(next);
  }

  function remove(index: number) {
    rowIds.current = rowIds.current.filter((_, position) => position !== index);
    onChange(entries.filter((_, position) => position !== index));
  }

  return (
    <div className="flex flex-col gap-3">
      <h2 className="text-body font-medium">Messages</h2>
      <ol aria-label="Messages" className="flex flex-col gap-3">
        {entries.map((entry, index) => {
          const number = index + 1;
          const noun = isPlaceholder(entry) ? "placeholder" : "message";
          const error = errors[index];
          return (
            <li key={rowIds.current[index]} className="flex flex-col gap-1.5 rounded-md border p-3">
              <div className="flex items-center gap-2">
                {isPlaceholder(entry) ? (
                  <>
                    <Badge variant="secondary">Placeholder</Badge>
                    <Input
                      aria-label={`Name of placeholder ${number}`}
                      className="h-7 w-48 text-body"
                      value={entry.name}
                      onChange={(event) => {
                        const name = event.target.value;
                        replaceAt(
                          index,
                          { type: "placeholder", name },
                          { from: entry.name, to: name },
                        );
                      }}
                    />
                  </>
                ) : (
                  <Select
                    value={entry.role}
                    onValueChange={(value) => {
                      const role = value as Role;
                      replaceAt(index, {
                        role,
                        // A tool response needs its id; the editor starts one for the user to fill.
                        parts:
                          role === "tool" && entry.role !== "tool"
                            ? [{ type: "tool_call_response", id: "", response: "" }]
                            : entry.parts,
                      });
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
                )}
                <div className="ml-auto flex shrink-0 items-center gap-1">
                  <Button
                    type="button"
                    variant="ghost"
                    size="icon"
                    aria-label={`Move ${noun} ${number} up`}
                    disabled={index === 0}
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
                    disabled={index === entries.length - 1}
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
                    onClick={() => {
                      remove(index);
                    }}
                  >
                    <Trash2 aria-hidden="true" />
                  </Button>
                </div>
              </div>
              {isPlaceholder(entry) ? null : editsAsParts(entry) ? (
                <PartsEditor
                  number={number}
                  parts={entry.parts}
                  onChange={(parts) => {
                    replaceAt(index, { role: entry.role, parts });
                  }}
                />
              ) : (
                <Textarea
                  aria-label={`Text of message ${number}`}
                  className="font-mono text-body"
                  spellCheck={false}
                  rows={3}
                  value={textOfParts(entry.parts)}
                  aria-invalid={error ? true : undefined}
                  onChange={(event) => {
                    replaceAt(index, {
                      role: entry.role,
                      parts: [{ type: "text", content: event.target.value }],
                    });
                  }}
                />
              )}
              {error ? <p className="text-caption text-destructive">{error}</p> : null}
            </li>
          );
        })}
      </ol>
      <div className="flex flex-wrap gap-2">
        <Button
          type="button"
          variant="outline"
          size="sm"
          onClick={() => {
            rowIds.current = [...rowIds.current, uuidv7()];
            onChange([...entries, { role: "user", parts: [{ type: "text", content: "" }] }]);
          }}
        >
          Add message
        </Button>
        <Button
          type="button"
          variant="outline"
          size="sm"
          onClick={() => {
            rowIds.current = [...rowIds.current, uuidv7()];
            onChange([...entries, { type: "placeholder", name: "history" }]);
          }}
        >
          Add placeholder
        </Button>
      </div>
    </div>
  );
});
