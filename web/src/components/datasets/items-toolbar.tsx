import { type KeyboardEvent, useEffect, useRef, useState } from "react";
import { Input } from "@/components/ui/input";

/** Quiet time, in ms, after the last keystroke before the search is applied. */
const SEARCH_DEBOUNCE_MS = 300;

/**
 * The items search box. Typing is applied after a short pause, and Enter applies it at once; the
 * applied text is trimmed, and `undefined` when it is blank. `value` is the applied text, so a
 * change from outside (back, a link) replaces the box's text.
 */
export function ItemSearch({
  value,
  onChange,
}: {
  value: string | undefined;
  onChange: (q: string | undefined) => void;
}) {
  const [draft, setDraft] = useState(value ?? "");
  const applied = useRef(value);
  const timer = useRef<number | undefined>(undefined);

  useEffect(() => {
    if (value !== applied.current) {
      applied.current = value;
      setDraft(value ?? "");
    }
  }, [value]);

  useEffect(() => () => window.clearTimeout(timer.current), []);

  function apply(text: string) {
    window.clearTimeout(timer.current);
    timer.current = undefined;
    const next = text.trim() === "" ? undefined : text.trim();
    applied.current = next;
    onChange(next);
  }

  function onKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    if (event.key === "Enter") {
      event.preventDefault();
      apply(draft);
    }
  }

  return (
    <Input
      type="search"
      aria-label="Search items"
      placeholder="Search items"
      // Capped so a typed search is always one the server accepts (1 to 200 characters).
      maxLength={200}
      value={draft}
      className="h-7 w-64 shrink-0 text-body md:text-body"
      onChange={(event) => {
        const text = event.target.value;
        setDraft(text);
        window.clearTimeout(timer.current);
        timer.current = window.setTimeout(() => {
          apply(text);
        }, SEARCH_DEBOUNCE_MS);
      }}
      onKeyDown={onKeyDown}
    />
  );
}
