import { diffLines } from "@/lib/line-diff";

/** The line diff of two texts: removed lines in red, added lines in green, the rest muted. */
export function LineDiffView({
  before,
  after,
  label,
}: {
  before: string;
  after: string;
  label: string;
}) {
  const lines = diffLines(before, after);
  if (lines.every((line) => line.op === "same")) {
    return <p className="text-body text-muted-foreground">No change.</p>;
  }
  return (
    <pre
      aria-label={label}
      className="overflow-x-auto rounded-md border font-mono text-body whitespace-pre-wrap"
    >
      {lines.map((line, index) => (
        <div
          // The lines of one diff never reorder, so their position is a stable key.
          // biome-ignore lint/suspicious/noArrayIndexKey: a diff is rendered once, in order
          key={index}
          data-op={line.op}
          className={
            line.op === "added"
              ? "bg-emerald-500/10"
              : line.op === "removed"
                ? "bg-red-500/10"
                : "text-muted-foreground"
          }
        >
          {line.op === "added" ? "+ " : line.op === "removed" ? "- " : "  "}
          {line.text}
        </div>
      ))}
    </pre>
  );
}
