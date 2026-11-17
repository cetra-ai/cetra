import { Loader2 } from "lucide-react";
import { useState } from "react";
import { api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { FormError } from "@/components/form/form-error";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { exportName, saveBlob } from "@/lib/sql/export";

type DatasetExportFormat = "jsonl" | "csv";

const FALLBACK = "The export could not be downloaded";

/** Downloads one version of the dataset as JSONL or CSV (D-J-7). Any reader may export. */
export function ExportMenu({ datasetId, version }: { datasetId: string; version: number }) {
  const { org, project } = useProject();
  const [error, setError] = useState<string | undefined>();
  const [pending, setPending] = useState(false);

  async function download(format: DatasetExportFormat) {
    setError(undefined);
    setPending(true);
    try {
      const {
        data,
        error: failure,
        response,
      } = await api.GET("/api/v1/projects/{project}/datasets/{dataset}/export", {
        params: {
          path: { project: projectRef(org, project), dataset: datasetId },
          query: { version, format },
        },
        parseAs: "blob",
      });
      if (failure !== undefined || !response.ok) {
        setError(problemMessage(failure, FALLBACK));
        return;
      }
      const name = exportName(
        response.headers.get("Content-Disposition"),
        `${datasetId}-v${version}.${format}`,
      );
      saveBlob(data ?? new Blob([]), name);
    } catch {
      setError(FALLBACK);
    } finally {
      setPending(false);
    }
  }

  return (
    <div className="flex flex-col items-start gap-1">
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button type="button" variant="outline" disabled={pending}>
            {pending ? <Loader2 className="animate-spin" aria-hidden="true" /> : null}
            {pending ? "Exporting…" : "Export"}
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent>
          <DropdownMenuItem
            onSelect={() => {
              void download("jsonl");
            }}
          >
            JSONL
          </DropdownMenuItem>
          <DropdownMenuItem
            onSelect={() => {
              void download("csv");
            }}
          >
            CSV
          </DropdownMenuItem>
          <p className="max-w-64 px-2 pb-1.5 text-caption text-muted-foreground">
            Open it in a spreadsheet as text: cells are not escaped against formulas.
          </p>
        </DropdownMenuContent>
      </DropdownMenu>
      <FormError message={error} />
    </div>
  );
}
