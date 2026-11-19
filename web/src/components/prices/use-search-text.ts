import { useEffect, useRef, useState } from "react";

const SEARCH_DEBOUNCE_MS = 300;

/**
 * A search box's text, with the URL's `q` written after a pause. The text stays as typed (a
 * trailing space survives) and follows `q` when it changes from elsewhere, for example by back.
 */
export function useSearchText(
  q: string | undefined,
  commit: (next: string | undefined) => void,
): [string, (text: string) => void] {
  const [text, setText] = useState(q ?? "");
  const commitRef = useRef(commit);
  useEffect(() => {
    commitRef.current = commit;
  });

  useEffect(() => {
    setText((current) => (current.trim() === (q ?? "") ? current : (q ?? "")));
  }, [q]);

  useEffect(() => {
    const timer = setTimeout(() => {
      const next = text.trim() || undefined;
      if (next !== q) {
        commitRef.current(next);
      }
    }, SEARCH_DEBOUNCE_MS);
    return () => {
      clearTimeout(timer);
    };
  }, [text, q]);

  return [text, setText];
}
