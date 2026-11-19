import { useQueryClient } from "@tanstack/react-query";
import { cn } from "cn";
import { Loader2 } from "lucide-react";
import { type ChangeEvent, useState } from "react";
import { api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { datasetWriteKeys } from "@/api/queries-datasets";
import type { Schema } from "@/api/types";
import { FormError } from "@/components/form/form-error";
import { useProject } from "@/components/shell/project-context";
import { Button, buttonVariants } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  defaultTarget,
  IMPORT_MAX_BYTES,
  type ImportFormat,
  type ImportTarget,
  importFormatOf,
  mappingProblems,
  PREVIEW_BYTES,
  previewJson,
} from "@/lib/datasets";
import { formatBytes } from "@/lib/format";

type Step = "file" | "preview" | "result";

/** The target choices of the mapping table; a keyed choice takes a key. */
type Kind = "input" | "input.key" | "expected" | "expected.key" | "metadata.key" | "ignore";

type MappingRow = { column: string; kind: Kind; key: string };

const KIND_OPTIONS: { kind: Kind; label: string }[] = [
  { kind: "input", label: "Whole input" },
  { kind: "input.key", label: "Input key" },
  { kind: "expected", label: "Whole expected output" },
  { kind: "expected.key", label: "Expected output key" },
  { kind: "metadata.key", label: "Metadata key" },
  { kind: "ignore", label: "Ignore" },
];

/** The table row of a column, from its default target (D-J-6). */
function rowOf(column: string): MappingRow {
  const target = defaultTarget(column);
  if (target === "input" || target === "expected" || target === "ignore") {
    return { column, kind: target, key: column };
  }
  const dot = target.indexOf(".");
  const base = target.slice(0, dot);
  return { column, kind: `${base}.key` as Kind, key: target.slice(dot + 1) };
}

/** The target a row sends: `input`, or `<base>.<key>` for a keyed choice. */
function targetOf(row: MappingRow): ImportTarget {
  if (row.kind.endsWith(".key")) {
    return `${row.kind.slice(0, -".key".length)}.${row.key}` as ImportTarget;
  }
  return row.kind as ImportTarget;
}

/** A row error the wizard lists. `row` is undefined for an error that is not about a row (a mapping). */
type ListedError = { row: number | undefined; message: string };

const PREVIEW_ERRORS_SHOWN = 5;

/** Imports a CSV or JSONL file into a new version of the dataset (D-J-6). */
export function ImportDialog({
  datasetId,
  open,
  onOpenChange,
}: {
  datasetId: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const [busy, setBusy] = useState(false);
  // A chosen file is work the user would lose, so Escape and a click outside do not close it.
  const [dirty, setDirty] = useState(false);
  const keepOpen = busy || dirty;

  function close() {
    setDirty(false);
    onOpenChange(false);
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && !busy) {
          close();
        }
      }}
    >
      <DialogContent
        className="max-h-[90vh] overflow-y-auto sm:max-w-3xl"
        onInteractOutside={(event) => {
          if (keepOpen) {
            event.preventDefault();
          }
        }}
        onEscapeKeyDown={(event) => {
          if (keepOpen) {
            event.preventDefault();
          }
        }}
      >
        <DialogHeader>
          <DialogTitle>Import items</DialogTitle>
          <DialogDescription>Adds the valid rows of a file as a new version.</DialogDescription>
        </DialogHeader>
        {open ? (
          <ImportWizard
            datasetId={datasetId}
            setBusy={setBusy}
            setDirty={setDirty}
            onClose={close}
          />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

function ImportWizard({
  datasetId,
  setBusy,
  setDirty,
  onClose,
}: {
  datasetId: string;
  setBusy: (busy: boolean) => void;
  setDirty: (dirty: boolean) => void;
  onClose: () => void;
}) {
  const { org, project } = useProject();
  const queryClient = useQueryClient();
  const [step, setStep] = useState<Step>("file");
  const [fileError, setFileError] = useState<string | undefined>();
  const [reading, setReading] = useState(false);
  const [file, setFile] = useState<File | undefined>();
  const [format, setFormat] = useState<ImportFormat | undefined>();
  const [preview, setPreview] = useState<Schema<"ImportPreview"> | undefined>();
  const [mapping, setMapping] = useState<MappingRow[]>([]);
  const [importing, setImporting] = useState(false);
  const [importError, setImportError] = useState<string | undefined>();
  const [listed, setListed] = useState<ListedError[]>([]);
  const [result, setResult] = useState<Schema<"ImportResult"> | undefined>();

  const path = { project: projectRef(org, project), dataset: datasetId };
  // A JSONL file has no mapping (D-J-6), so there is nothing to check.
  const problems =
    format === "csv"
      ? mappingProblems(mapping.map((row) => ({ column: row.column, target: targetOf(row) })))
      : [];

  async function choose(event: ChangeEvent<HTMLInputElement>) {
    const input = event.target;
    const chosen = input.files?.[0];
    if (!chosen) {
      return;
    }
    setFileError(undefined);
    const detected = importFormatOf(chosen.name);
    if (detected === undefined) {
      setFileError("Choose a .csv or .jsonl file");
      return;
    }
    if (chosen.size > IMPORT_MAX_BYTES) {
      setFileError("The file is larger than 100 MiB");
      return;
    }
    setReading(true);
    try {
      // The browser sends only the head of a large file; `partial` drops its last, possibly cut, line.
      const head = chosen.slice(0, PREVIEW_BYTES);
      const { data, error } = await api.POST(
        "/api/v1/projects/{project}/datasets/{dataset}/import/preview",
        {
          params: {
            path,
            query: { format: detected, partial: chosen.size > PREVIEW_BYTES },
          },
          body: head as never,
          bodySerializer: (body) => body,
          headers: { "Content-Type": "application/octet-stream" },
        },
      );
      if (error !== undefined || data === undefined) {
        setFileError(problemMessage(error, "The file could not be read"));
        return;
      }
      setFile(chosen);
      setFormat(detected);
      setPreview(data);
      setMapping(detected === "csv" ? data.columns.map(rowOf) : []);
      setDirty(true);
      setStep("preview");
    } catch {
      setFileError("The file could not be read");
    } finally {
      setReading(false);
      // Clear the input so choosing the same file again fires a change.
      input.value = "";
    }
  }

  function updateRow(index: number, change: Partial<Omit<MappingRow, "column">>) {
    setMapping((rows) => rows.map((row, at) => (at === index ? { ...row, ...change } : row)));
  }

  function listErrors(problem: ReturnType<typeof problemOf>): ListedError[] {
    return (problem?.errors ?? []).map((item) => {
      const match = /^rows\[(\d+)\]$/.exec(item.field);
      return { row: match ? Number(match[1]) : undefined, message: item.message };
    });
  }

  async function runImport() {
    if (!file || !format) {
      return;
    }
    setImportError(undefined);
    setListed([]);
    setImporting(true);
    setBusy(true);
    try {
      const form = new FormData();
      form.append("file", file);
      if (format === "csv") {
        const columns = mapping.map((row) => ({ column: row.column, target: targetOf(row) }));
        form.append(
          "mapping",
          new Blob([JSON.stringify({ columns })], { type: "application/json" }),
        );
      }
      const { data, error, response } = await api.POST(
        "/api/v1/projects/{project}/datasets/{dataset}/import",
        {
          params: { path, query: { format } },
          body: form as never,
          bodySerializer: (body) => body,
        },
      );
      if (error !== undefined || data === undefined) {
        setImportError(importFailure(response.status, error));
        setListed(listErrors(problemOf(error)));
        return;
      }
      setResult(data);
      setStep("result");
      for (const key of datasetWriteKeys(org, project, datasetId)) {
        void queryClient.invalidateQueries({ queryKey: key });
      }
    } catch {
      setImportError("The import failed");
    } finally {
      setImporting(false);
      setBusy(false);
    }
  }

  if (step === "result" && result) {
    return (
      <div className="flex flex-col gap-4">
        <p className="text-body">{`Imported ${result.imported} items as version ${result.version}.`}</p>
        {result.error_count > 0 ? (
          <p className="text-body">{`${result.error_count} rows were skipped.`}</p>
        ) : null}
        {result.errors.length > 0 ? (
          <Table aria-label="Row errors">
            <TableHeader>
              <TableRow>
                <TableHead>Row</TableHead>
                <TableHead>Message</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {result.errors.map((item) => (
                <TableRow key={item.row}>
                  <TableCell>{item.row}</TableCell>
                  <TableCell>{item.message}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        ) : null}
        {result.error_count > result.errors.length ? (
          <p className="text-caption text-muted-foreground">
            {`Showing the first ${result.errors.length} errors.`}
          </p>
        ) : null}
        <DialogFooter className="sticky bottom-0 bg-popover pt-2">
          <Button type="button" onClick={onClose}>
            Done
          </Button>
        </DialogFooter>
      </div>
    );
  }

  if (step === "preview" && file && format && preview) {
    const shownErrors = preview.errors.slice(0, PREVIEW_ERRORS_SHOWN);
    const hiddenErrors = preview.errors.length - shownErrors.length;
    return (
      <div className="flex flex-col gap-4">
        <p className="text-caption text-muted-foreground">
          {`${file.name} · ${formatBytes(file.size)} · ${format.toUpperCase()}`}
        </p>
        {preview.truncated ? (
          <p className="text-caption text-muted-foreground">Showing the first rows of the file.</p>
        ) : null}
        {shownErrors.length > 0 ? (
          <div className="flex flex-col gap-1 text-caption text-destructive">
            {shownErrors.map((item) => (
              <p key={item.row}>{`Row ${item.row}: ${item.message}`}</p>
            ))}
            {hiddenErrors > 0 ? <p>{`and ${hiddenErrors} more`}</p> : null}
          </div>
        ) : null}
        <Table aria-label="Preview">
          <TableHeader>
            <TableRow>
              {format === "csv" ? (
                preview.columns.map((column) => <TableHead key={column}>{column}</TableHead>)
              ) : (
                <TableHead>Row</TableHead>
              )}
            </TableRow>
          </TableHeader>
          <TableBody>
            {preview.rows.map((row, index) =>
              format === "csv" ? (
                // biome-ignore lint/suspicious/noArrayIndexKey: the preview is fixed for the step and never reorders
                <TableRow key={`row-${index}`}>
                  {(row as unknown[]).map((cell, at) => (
                    // biome-ignore lint/suspicious/noArrayIndexKey: a preview cell never reorders
                    <TableCell key={`cell-${at}`}>{previewJson(cell, 200)}</TableCell>
                  ))}
                </TableRow>
              ) : (
                // biome-ignore lint/suspicious/noArrayIndexKey: the preview is fixed for the step and never reorders
                <TableRow key={`row-${index}`}>
                  <TableCell className="font-mono text-caption">{previewJson(row, 200)}</TableCell>
                </TableRow>
              ),
            )}
          </TableBody>
        </Table>
        {format === "csv" ? (
          <Table aria-label="Mapping">
            <TableHeader>
              <TableRow>
                <TableHead>Column</TableHead>
                <TableHead>Target</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {mapping.map((row, index) => (
                <TableRow key={row.column}>
                  <TableCell>{row.column}</TableCell>
                  <TableCell>
                    <div className="flex flex-wrap items-center gap-2">
                      <Select
                        value={row.kind}
                        onValueChange={(value) => {
                          updateRow(index, { kind: value as Kind });
                        }}
                      >
                        <SelectTrigger aria-label={`Target of ${row.column}`} className="w-fit">
                          <SelectValue />
                        </SelectTrigger>
                        <SelectContent>
                          {KIND_OPTIONS.map((option) => (
                            <SelectItem key={option.kind} value={option.kind}>
                              {option.label}
                            </SelectItem>
                          ))}
                        </SelectContent>
                      </Select>
                      {row.kind.endsWith(".key") ? (
                        <Input
                          aria-label={`Key of ${row.column}`}
                          className="w-40"
                          value={row.key}
                          onChange={(event) => {
                            updateRow(index, { key: event.target.value });
                          }}
                        />
                      ) : null}
                    </div>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        ) : (
          <p className="text-caption text-muted-foreground">
            Each line must be an object with input and, optionally, expected_output and metadata.
          </p>
        )}
        {problems.length > 0 ? (
          <div role="alert" className="flex flex-col gap-1 text-caption text-destructive">
            {problems.map((problem) => (
              <p key={problem}>{problem}</p>
            ))}
          </div>
        ) : null}
        {importError ? <FormError message={importError} /> : null}
        {listed.length > 0 ? (
          <Table aria-label="Row errors">
            <TableHeader>
              <TableRow>
                <TableHead>Row</TableHead>
                <TableHead>Message</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {listed.map((item) => (
                <TableRow key={`${item.row ?? "mapping"}-${item.message}`}>
                  <TableCell>{item.row ?? "Mapping"}</TableCell>
                  <TableCell>{item.message}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        ) : null}
        <DialogFooter className="sticky bottom-0 bg-popover pt-2">
          <Button
            type="button"
            variant="outline"
            disabled={importing}
            onClick={() => {
              setFile(undefined);
              setFormat(undefined);
              setPreview(undefined);
              setImportError(undefined);
              setListed([]);
              setDirty(false);
              setStep("file");
            }}
          >
            Back
          </Button>
          <Button
            type="button"
            disabled={importing || problems.length > 0}
            onClick={() => {
              void runImport();
            }}
          >
            {importing ? <Loader2 className="animate-spin" aria-hidden="true" /> : null}
            Import
          </Button>
        </DialogFooter>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-col items-start gap-2">
        <label
          className={cn(
            buttonVariants({ variant: "outline" }),
            "cursor-pointer focus-within:outline-2 focus-within:outline-ring focus-within:outline-offset-2 focus-within:outline-solid",
            reading && "pointer-events-none opacity-50",
          )}
        >
          <input
            type="file"
            accept=".csv,.jsonl,.ndjson"
            className="sr-only"
            disabled={reading}
            onChange={(event) => {
              void choose(event);
            }}
          />
          Choose a CSV or JSONL file
        </label>
        {reading ? <p className="text-caption text-muted-foreground">Reading the file…</p> : null}
      </div>
      <FormError message={fileError} />
    </div>
  );
}

/** The message of a refused import, by status (D-J-6). */
function importFailure(status: number, error: unknown): string {
  if (status === 413) {
    return "The file is larger than 100 MiB";
  }
  if (status === 409) {
    return problemMessage(error, "The dataset would hold more than 50000 items");
  }
  if (status === 400) {
    return problemMessage(error, "No row could be imported");
  }
  return problemMessage(error, "The import failed");
}
