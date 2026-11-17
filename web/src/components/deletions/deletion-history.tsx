import { History } from "lucide-react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { deletionsInit } from "@/api/queries-settings";
import { EmptyState } from "@/components/empty-state";
import { FormError } from "@/components/form/form-error";
import { IdText } from "@/components/id-text";
import { SettingsCard } from "@/components/settings/settings-card";
import { type PillTone, StatusPill } from "@/components/settings/status-pill";
import { useProject } from "@/components/shell/project-context";
import { TimeText } from "@/components/time-text";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  deletionErrorLabel,
  deletionKindLabel,
  deletionPollMs,
  deletionStatusLabel,
  deletionSubject,
  isOpenDeletion,
} from "@/lib/deletions";

/** Who asked, as a label (`requested_by.kind`). */
function actorLabel(kind: "user" | "api_key" | "control_plane"): string {
  switch (kind) {
    case "api_key":
      return "API key";
    case "control_plane":
      return "Control plane";
    case "user":
      return "Member";
  }
}

/** The kind word `IdText` shows before the id of the requester. */
function actorIdKind(kind: "user" | "api_key" | "control_plane"): string {
  switch (kind) {
    case "api_key":
      return "API key";
    case "control_plane":
      return "control plane";
    case "user":
      return "user";
  }
}

/** Pill colour per status: finished work is ok, failed is an error, the rest are in progress. */
function statusTone(status: "resolving" | "pending" | "applied" | "failed"): PillTone {
  switch (status) {
    case "applied":
      return "ok";
    case "failed":
      return "error";
    case "pending":
      return "warn";
    case "resolving":
      return "info";
  }
}

/** The project's deletions, newest first. Refetches while one is open. */
export function DeletionHistory() {
  const { org, project } = useProject();
  const list = $api.useQuery(
    "get",
    "/api/v1/projects/{project}/deletions",
    deletionsInit(org, project),
    {
      refetchInterval: (query) =>
        query.state.data?.data.some(isOpenDeletion) ? deletionPollMs() : false,
    },
  );

  const rows = list.data?.data ?? [];

  return (
    <SettingsCard title="Deletion history">
      {list.data === undefined && list.error ? (
        <FormError message={problemMessage(list.error, "Could not load the deletions.")} />
      ) : null}
      {list.data === undefined ? null : (
        <>
          {list.error ? (
            <FormError message={problemMessage(list.error, "Could not load the deletions.")} />
          ) : null}
          {rows.length === 0 ? (
            <EmptyState icon={History} title="No deletions yet" />
          ) : (
            <Table aria-label="Deletions">
              <TableHeader>
                <TableRow>
                  <TableHead>Requested</TableHead>
                  <TableHead>What</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead>By</TableHead>
                  <TableHead>Applied</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {rows.map((deletion) => (
                  <TableRow key={deletion.id}>
                    <TableCell>
                      <TimeText value={deletion.created_at} style="datetime" />
                    </TableCell>
                    <TableCell>
                      <div className="flex flex-col">
                        <span>{deletionKindLabel(deletion.kind)}</span>
                        <span className="text-caption text-muted-foreground">
                          {deletionSubject(deletion)}
                        </span>
                      </div>
                    </TableCell>
                    <TableCell>
                      <div className="flex flex-col items-start gap-1">
                        <StatusPill tone={statusTone(deletion.status)}>
                          {deletionStatusLabel(deletion.status)}
                        </StatusPill>
                        {deletion.error ? (
                          <span className="text-caption text-muted-foreground">
                            {deletionErrorLabel(deletion.error)}
                          </span>
                        ) : null}
                      </div>
                    </TableCell>
                    <TableCell>
                      <div className="flex flex-col">
                        <span>{actorLabel(deletion.requested_by.kind)}</span>
                        <IdText
                          value={deletion.requested_by.id}
                          kind={actorIdKind(deletion.requested_by.kind)}
                        />
                      </div>
                    </TableCell>
                    <TableCell>
                      {deletion.applied_at ? (
                        <TimeText value={deletion.applied_at} style="datetime" />
                      ) : (
                        "—"
                      )}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </>
      )}
    </SettingsCard>
  );
}
