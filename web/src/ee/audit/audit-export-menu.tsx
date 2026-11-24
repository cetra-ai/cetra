// SPDX-License-Identifier: Elastic-2.0
import { Loader2 } from "lucide-react";
import { useState } from "react";
import { api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { FormError } from "@/components/form/form-error";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { AuditQuery, AuditScope } from "@/lib/search/audit";
import { exportName, saveBlob } from "@/lib/sql/export";

type AuditExportFormat = "csv" | "ndjson";

const FALLBACK = "The export could not be downloaded";

/** Downloads the filtered audit records as CSV or NDJSON. The server records the export first. */
export function AuditExportMenu({ scope, query }: { scope: AuditScope; query: AuditQuery }) {
  const [error, setError] = useState<string | undefined>();
  const [pending, setPending] = useState(false);

  async function download(format: AuditExportFormat) {
    setError(undefined);
    setPending(true);
    try {
      // One call per literal path, so openapi-fetch types each one.
      const result =
        scope === "instance"
          ? await api.GET("/api/v1/admin/audit-events/export", {
              params: { query: { ...query, format } },
              parseAs: "blob",
            })
          : await api.GET("/api/v1/orgs/{org}/audit-events/export", {
              params: { path: { org: scope.org }, query: { ...query, format } },
              parseAs: "blob",
            });
      const { data, error: failure, response } = result;
      if (failure !== undefined || !response.ok) {
        setError(problemMessage(failure, FALLBACK));
        return;
      }
      const name = exportName(response.headers.get("Content-Disposition"), `audit-log.${format}`);
      saveBlob(data ?? new Blob([]), name);
    } catch {
      setError(FALLBACK);
    } finally {
      setPending(false);
    }
  }

  return (
    <div className="flex flex-col items-end gap-1">
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button type="button" variant="outline" size="sm" disabled={pending}>
            {pending ? <Loader2 className="animate-spin" aria-hidden="true" /> : null}
            {pending ? "Exporting…" : "Export"}
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent>
          <DropdownMenuItem
            onSelect={() => {
              void download("csv");
            }}
          >
            Export CSV
          </DropdownMenuItem>
          <DropdownMenuItem
            onSelect={() => {
              void download("ndjson");
            }}
          >
            Export NDJSON
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
      <FormError message={error} />
    </div>
  );
}
