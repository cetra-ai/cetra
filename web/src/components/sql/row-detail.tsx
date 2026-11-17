import { useState } from "react";
import type { Schema } from "@/api/types";
import { CopyButton } from "@/components/copy-button";
import type { SqlRow } from "@/components/sql/result-grid";
import { Button } from "@/components/ui/button";
import { Sheet, SheetContent, SheetHeader, SheetTitle } from "@/components/ui/sheet";

/** Strings longer than this show in a preformatted block, not on the line. */
const LONG_TEXT = 120;

/** Text a copy button puts on the clipboard: strings as they are, objects as pretty JSON. */
function rawText(value: unknown): string {
  if (value === null || value === undefined) {
    return "";
  }
  if (typeof value === "string") {
    return value;
  }
  if (typeof value === "object") {
    return JSON.stringify(value, null, 2);
  }
  return String(value);
}

function FieldValue({ value }: { value: unknown }) {
  if (value === null || value === undefined) {
    return <p className="mt-1 font-mono text-[12px] text-muted-foreground">NULL</p>;
  }
  if (typeof value === "object") {
    return (
      <pre className="mt-1 max-h-64 overflow-auto font-mono text-[12px] break-words whitespace-pre-wrap">
        {JSON.stringify(value, null, 2)}
      </pre>
    );
  }
  const text = String(value);
  if (typeof value === "string" && (value.length > LONG_TEXT || value.includes("\n"))) {
    return <pre className="mt-1 font-mono text-[12px] break-words whitespace-pre-wrap">{text}</pre>;
  }
  return <p className="mt-1 font-mono text-[12px] break-words">{text}</p>;
}

function FieldEntry({ name, type, value }: { name: string; type: string; value: unknown }) {
  return (
    <div className="border-b py-2">
      <div className="flex items-center gap-2">
        <span className="min-w-0 truncate font-mono text-[12px]">{name}</span>
        <span className="text-[11px] text-muted-foreground">{type}</span>
        <span className="ml-auto">
          <CopyButton value={rawText(value)} label={name} />
        </span>
      </div>
      <FieldValue value={value} />
    </div>
  );
}

/**
 * The row opened from the results grid: every column in result order, with its type,
 * value and a copy button, plus a copy of the whole row as JSON.
 */
export function RowDetailSheet({
  row,
  columns,
  onClose,
}: {
  row: SqlRow | undefined;
  columns: Schema<"SqlQueryResult">["columns"];
  onClose: () => void;
}) {
  // The last opened row stays rendered while the sheet slides closed; without it the content
  // would unmount at once and the sheet would go blank during the close animation.
  const [shown, setShown] = useState(row);
  if (row && row !== shown) {
    setShown(row);
  }

  return (
    <Sheet
      open={row !== undefined}
      onOpenChange={(open) => {
        if (!open) {
          onClose();
        }
      }}
    >
      <SheetContent aria-describedby={undefined} className="gap-0 overflow-y-auto">
        {shown ? (
          <>
            <SheetHeader className="gap-3">
              <SheetTitle className="text-[13px]">{`Row ${shown.index + 1}`}</SheetTitle>
              <div>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={() => {
                    void navigator.clipboard
                      .writeText(JSON.stringify(shown.values, null, 2))
                      .catch(() => {
                        // The clipboard can reject the write. Leave the button as it is.
                      });
                  }}
                >
                  Copy row as JSON
                </Button>
              </div>
            </SheetHeader>
            <div className="px-4 pb-4">
              {columns.map((column, index) => (
                // Keyed by position: duplicate column names (`SELECT 1 AS a, 2 AS a`) must not
                // share a React key. The values object holds one value per name, so each
                // entry shows what `values[name]` holds, as before.
                <FieldEntry
                  // biome-ignore lint/suspicious/noArrayIndexKey: the columns of a result are fixed and never reorder, and names can repeat, so the position is the stable identity
                  key={index}
                  name={column.name}
                  type={column.type}
                  value={
                    Object.hasOwn(shown.values, column.name) ? shown.values[column.name] : undefined
                  }
                />
              ))}
            </div>
          </>
        ) : null}
      </SheetContent>
    </Sheet>
  );
}
