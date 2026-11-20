import { SendHorizontal, Square, X } from "lucide-react";
import type { KeyboardEvent } from "react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import type { PageContext } from "@/lib/assistant/context";

export const MESSAGE_MAX_BYTES = 16384;
const COUNT_FROM_BYTES = 15000;
const CHIP_VALUE_MAX = 40;

export function byteLength(text: string): number {
  return new TextEncoder().encode(text).length;
}

/** A chip's value, cut to 40 characters for display. The full value is what gets sent. */
export function chipValue(value: string | number | boolean): string {
  const points = Array.from(String(value));
  return points.length > CHIP_VALUE_MAX
    ? `${points.slice(0, CHIP_VALUE_MAX).join("")}…`
    : points.join("");
}

/**
 * The message box. Enter sends and Shift+Enter adds a line. While a turn streams the Send button
 * becomes Stop. The page context shows as chips; `page` cannot be removed.
 */
export function Composer({
  value,
  onChange,
  onSend,
  onStop,
  streaming,
  context,
  onRemove,
}: {
  value: string;
  onChange: (value: string) => void;
  onSend: () => void;
  onStop: () => void;
  streaming: boolean;
  context: PageContext;
  onRemove: (key: string) => void;
}) {
  const bytes = byteLength(value);
  const tooLong = bytes > MESSAGE_MAX_BYTES;
  const canSend = !streaming && value.trim() !== "" && !tooLong;

  function onKeyDown(event: KeyboardEvent<HTMLTextAreaElement>) {
    if (event.key !== "Enter" || event.shiftKey || event.nativeEvent.isComposing) {
      return;
    }
    event.preventDefault();
    if (canSend) {
      onSend();
    }
  }

  return (
    <div className="flex shrink-0 flex-col gap-2 border-t p-3">
      {Object.entries(context).length > 0 ? (
        <ul aria-label="Page context" className="flex flex-wrap gap-1">
          {Object.entries(context).map(([key, value]) => (
            <li
              key={key}
              className="flex max-w-full items-center gap-1 rounded-sm border bg-muted px-1.5 py-0.5 text-caption"
            >
              <span className="truncate">
                <span className="font-medium">{key}</span>: {chipValue(value)}
              </span>
              {key === "page" ? null : (
                <button
                  type="button"
                  aria-label={`Remove ${key}`}
                  className="rounded-sm text-muted-foreground hover:text-foreground"
                  onClick={() => {
                    onRemove(key);
                  }}
                >
                  <X aria-hidden="true" className="size-3" />
                </button>
              )}
            </li>
          ))}
        </ul>
      ) : null}
      <div className="flex items-end gap-2">
        <Textarea
          aria-label="Message the assistant"
          value={value}
          rows={2}
          className="max-h-40 min-h-10 flex-1 resize-none"
          onChange={(event) => {
            onChange(event.target.value);
          }}
          onKeyDown={onKeyDown}
        />
        {streaming ? (
          <Button type="button" variant="outline" size="icon" aria-label="Stop" onClick={onStop}>
            <Square />
          </Button>
        ) : (
          <Button type="button" size="icon" aria-label="Send" disabled={!canSend} onClick={onSend}>
            <SendHorizontal />
          </Button>
        )}
      </div>
      {bytes > COUNT_FROM_BYTES ? (
        <p
          className={
            tooLong ? "text-caption text-destructive" : "text-caption text-muted-foreground"
          }
        >
          {bytes} / {MESSAGE_MAX_BYTES} bytes
        </p>
      ) : null}
    </div>
  );
}
