import { SendHorizontal, Square, X } from "lucide-react";
import type { KeyboardEvent, ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import type { PageContext } from "@/lib/assistant/context";
import { fieldLabel, PAGE_LABEL, placeholderOf } from "@/lib/assistant/starters";

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
 * The message box: one bordered box with the text, the page context as chips and Send. Enter sends
 * and Shift+Enter adds a line. While a turn streams Send becomes Stop. `page` cannot be removed.
 * `footer` is the line under the box (the connection).
 */
export function Composer({
  value,
  onChange,
  onSend,
  onStop,
  streaming,
  context,
  onRemove,
  footer,
}: {
  value: string;
  onChange: (value: string) => void;
  onSend: () => void;
  onStop: () => void;
  streaming: boolean;
  context: PageContext;
  onRemove: (key: string) => void;
  footer?: ReactNode;
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
    <div className="flex shrink-0 flex-col gap-1.5 p-3">
      <div className="flex flex-col rounded-lg border border-input bg-input/30 shadow-xs focus-within:border-ring focus-within:ring-3 focus-within:ring-ring/50">
        <Textarea
          aria-label="Message the assistant"
          placeholder={placeholderOf(context.page)}
          value={value}
          rows={2}
          className="max-h-48 min-h-14 resize-none border-0 bg-transparent shadow-none focus-visible:ring-0 dark:bg-transparent"
          onChange={(event) => {
            onChange(event.target.value);
          }}
          onKeyDown={onKeyDown}
        />
        <div className="flex items-end gap-2 px-2 pb-2">
          <ul aria-label="Page context" className="flex min-w-0 flex-1 flex-wrap gap-1">
            {Object.entries(context).map(([key, item]) => (
              <li
                key={key}
                className="flex max-w-full items-center gap-1 rounded-sm bg-muted px-1.5 py-0.5 text-caption text-muted-foreground"
              >
                <span className="truncate">
                  {key === "page" ? (
                    <span className="font-medium text-foreground">{PAGE_LABEL[context.page]}</span>
                  ) : (
                    <>
                      <span className="font-medium text-foreground">{fieldLabel(key)}</span>{" "}
                      {chipValue(item)}
                    </>
                  )}
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
          {streaming ? (
            <Button
              type="button"
              variant="outline"
              size="icon-sm"
              aria-label="Stop"
              onClick={onStop}
            >
              <Square />
            </Button>
          ) : (
            <Button
              type="button"
              size="icon-sm"
              aria-label="Send"
              title="Send (Enter) · Shift+Enter for a new line"
              disabled={!canSend}
              onClick={onSend}
            >
              <SendHorizontal />
            </Button>
          )}
        </div>
      </div>
      <div className="flex items-center gap-2 text-caption text-muted-foreground">
        <div className="min-w-0 flex-1">{footer}</div>
        {bytes > COUNT_FROM_BYTES ? (
          <p className={tooLong ? "shrink-0 text-destructive" : "shrink-0"}>
            {bytes} / {MESSAGE_MAX_BYTES} bytes
          </p>
        ) : null}
      </div>
    </div>
  );
}
