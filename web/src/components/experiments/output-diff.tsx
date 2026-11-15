import type { ReactNode } from "react";
import { wordDiff } from "@/lib/word-diff";

/** At most this many characters of rendered diff text (D-J-17). */
const MAX_RENDERED_CHARS = 2000;
/** A candidate output too long to diff is shown cut to this many characters. */
const TOO_LONG_CHARS = 300;

/** A string as itself, `undefined` as "", anything else as its indented JSON. */
function textOf(value: unknown): string {
  if (typeof value === "string") {
    return value;
  }
  if (value === undefined) {
    return "";
  }
  return JSON.stringify(value, null, 2) ?? "";
}

function clip(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max)}…` : text;
}

/**
 * The candidate's output against the baseline's: word-level insertions and deletions (D-J-17).
 * Equal outputs show their text as "same as baseline"; a diff over 2,000 words shows the
 * candidate's text cut short with "Too long to diff".
 */
export function OutputDiff({ before, after }: { before: unknown; after: unknown }) {
  const beforeText = textOf(before);
  const afterText = textOf(after);
  if (beforeText === afterText) {
    return (
      <p className="whitespace-pre-wrap font-mono text-xs text-muted-foreground">
        <span className="sr-only">same as baseline</span>
        {clip(afterText, MAX_RENDERED_CHARS)}
      </p>
    );
  }

  const parts = wordDiff(beforeText, afterText);
  if (parts === undefined) {
    return (
      <p className="whitespace-pre-wrap font-mono text-xs">
        {clip(afterText, TOO_LONG_CHARS)}
        <span className="block text-muted-foreground">Too long to diff</span>
      </p>
    );
  }

  const nodes: ReactNode[] = [];
  let budget = MAX_RENDERED_CHARS;
  let truncated = false;
  for (const [index, part] of parts.entries()) {
    if (budget <= 0) {
      truncated = true;
      break;
    }
    const text = part.text.length > budget ? part.text.slice(0, budget) : part.text;
    if (text.length < part.text.length) {
      truncated = true;
    }
    budget -= text.length;
    if (part.kind === "added") {
      nodes.push(
        <ins key={index} className="bg-status-ok/15 no-underline">
          {text}
        </ins>,
      );
    } else if (part.kind === "removed") {
      nodes.push(
        <del key={index} className="bg-status-error/15">
          {text}
        </del>,
      );
    } else {
      nodes.push(<span key={index}>{text}</span>);
    }
    if (truncated) {
      break;
    }
  }

  return (
    <p className="whitespace-pre-wrap font-mono text-xs">
      {nodes}
      {truncated ? "…" : null}
    </p>
  );
}
