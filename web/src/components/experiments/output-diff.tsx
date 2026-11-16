import { type ReactNode, useMemo } from "react";
import { type DiffPart, wordDiff } from "@/lib/word-diff";

/** At most this many characters of rendered diff text (D-J-17). */
const MAX_RENDERED_CHARS = 2000;
/** A candidate output too long to diff is shown cut to this many characters. */
const TOO_LONG_CHARS = 300;
/** Unchanged text kept on each side of a change; longer unchanged runs are cut to it. */
const CONTEXT_CHARS = 40;

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

function noOutput(): ReactNode {
  return <p className="text-xs text-muted-foreground">No output</p>;
}

/**
 * An unchanged run, cut to its context: the start when a change comes before it, the end when a
 * change comes after it, with "…" for the text left out. A run of up to two contexts stays whole.
 */
function windowOf(text: string, afterChange: boolean, beforeChange: boolean): string {
  const keep = (afterChange ? CONTEXT_CHARS : 0) + (beforeChange ? CONTEXT_CHARS : 0);
  if (text.length <= keep) {
    return text;
  }
  if (afterChange && beforeChange) {
    return text.length <= 2 * CONTEXT_CHARS
      ? text
      : `${text.slice(0, CONTEXT_CHARS)}…${text.slice(-CONTEXT_CHARS)}`;
  }
  return afterChange ? `${text.slice(0, CONTEXT_CHARS)}…` : `…${text.slice(-CONTEXT_CHARS)}`;
}

/** The parts with each unchanged run cut to its context next to the changes. */
function windowParts(parts: readonly DiffPart[]): DiffPart[] {
  return parts.map((part, index) => {
    if (part.kind !== "same") {
      return part;
    }
    // A same part sits between changes or at an end; a change always precedes or follows it.
    const afterChange = index > 0;
    const beforeChange = index < parts.length - 1;
    return { kind: "same", text: windowOf(part.text, afterChange, beforeChange) };
  });
}

/** The diff of a candidate output against the baseline's, as rendered text. */
function diffNodes(before: unknown, after: unknown): ReactNode {
  if (after === undefined) {
    return noOutput();
  }
  const afterText = textOf(after);
  if (before === undefined) {
    return (
      <p className="whitespace-pre-wrap font-mono text-xs">
        {clip(afterText, MAX_RENDERED_CHARS)}
        <span className="block text-muted-foreground">No baseline output</span>
      </p>
    );
  }
  const beforeText = textOf(before);
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
  for (const [index, part] of windowParts(parts).entries()) {
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
          <span className="sr-only">added: </span>
          {text}
        </ins>,
      );
    } else if (part.kind === "removed") {
      nodes.push(
        <del key={index} className="bg-status-error/15">
          <span className="sr-only">removed: </span>
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

/**
 * The candidate's output against the baseline's: word-level insertions and deletions (D-J-17).
 * Equal outputs show their text as "same as baseline"; unchanged runs are cut to their context
 * so the changes fit the 2,000-character budget; a diff over 2,000 words shows the candidate's
 * text cut short with "Too long to diff". A missing baseline output shows the candidate's text
 * with "No baseline output"; a missing candidate output shows "No output".
 */
export function OutputDiff({ before, after }: { before: unknown; after: unknown }) {
  return useMemo(() => diffNodes(before, after), [before, after]);
}
