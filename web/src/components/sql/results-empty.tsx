import { SQL_EXAMPLES, type SqlExample } from "@/lib/sql/examples";
import { runShortcutLabel } from "@/lib/sql/format";

/** The first lines of a query, enough to tell the examples apart. */
const PREVIEW_LINES = 3;

/** Shown before the first run: how to run, and the examples to start from. */
export function ResultsEmpty({ onPick }: { onPick: (example: SqlExample) => void }) {
  const shortcut = runShortcutLabel(navigator.userAgent);
  return (
    <div className="mx-auto w-full max-w-4xl overflow-y-auto pt-4 text-[13px]">
      <h2 className="flex flex-wrap items-center gap-2 font-medium text-[13px]">
        <span>Run a query to see results</span>
        <kbd className="rounded border bg-muted px-1.5 py-0.5 font-mono font-normal text-[11px] text-muted-foreground">
          {shortcut}
        </kbd>
      </h2>
      <p className="mt-4 text-[12px] text-muted-foreground">Or start from an example:</p>
      <ul className="mt-2 grid gap-2 sm:grid-cols-2 xl:grid-cols-3">
        {SQL_EXAMPLES.map((example) => (
          <li key={example.name} className="min-w-0">
            <button
              type="button"
              onClick={() => onPick(example)}
              className="flex w-full min-w-0 flex-col gap-1.5 overflow-hidden rounded-md border p-3 text-left hover:bg-accent focus-visible:outline-hidden focus-visible:ring-1 focus-visible:ring-ring"
            >
              <span className="block w-full truncate font-medium text-[13px]">{example.name}</span>
              <span className="block h-12 w-full overflow-hidden whitespace-pre font-mono text-[11px] leading-4 text-muted-foreground">
                {example.query.split("\n").slice(0, PREVIEW_LINES).join("\n")}
              </span>
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}
