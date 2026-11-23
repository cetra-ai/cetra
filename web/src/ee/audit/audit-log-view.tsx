// SPDX-License-Identifier: Elastic-2.0
import { Link } from "@tanstack/react-router";
import { type ReactNode, useEffect, useMemo, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import type { Schema } from "@/api/types";
import { type Option, SelectField } from "@/components/alerts/select-field";
import { IdText } from "@/components/id-text";
import { TimeText } from "@/components/time-text";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { AuditExportMenu } from "@/ee/audit/audit-export-menu";
import { AuditRecordSheet } from "@/ee/audit/audit-record-sheet";
import {
  type AuditQuery,
  type AuditRange,
  type AuditScope,
  type AuditSearch,
  auditQuery,
} from "@/lib/search/audit";

type AuditEvent = Schema<"AuditEvent">;
type AuditRef = Schema<"AuditRef">;
type AuditActor = Schema<"AuditActor">;
type AuditRefStatus = Schema<"AuditRefStatus">;

const PAGE_DEBOUNCE_MS = 300;

const RANGE_OPTIONS: Option<AuditRange>[] = [
  { value: "24h", label: "Last 24 hours" },
  { value: "7d", label: "Last 7 days" },
  { value: "30d", label: "Last 30 days" },
  { value: "90d", label: "Last 90 days" },
  { value: "all", label: "All time" },
];

type OutcomeChoice = "all" | "success" | "failure";
const OUTCOME_OPTIONS: Option<OutcomeChoice>[] = [
  { value: "all", label: "Any outcome" },
  { value: "success", label: "Success" },
  { value: "failure", label: "Failure" },
];

/** Rounds `date` down to the minute, so a search's query key stays stable within the minute. */
function floorToMinute(date: Date): Date {
  return new Date(Math.floor(date.getTime() / 60_000) * 60_000);
}

/** The label of a record's object whose status is not `present`. */
function refStatusLabel(status: AuditRefStatus, kind: string): string {
  switch (status) {
    case "present":
      return kind;
    case "former_member":
      return "Former member";
    case "deleted":
      if (kind === "user") {
        return "Deleted user";
      }
      if (kind === "api_key") {
        return "Deleted key";
      }
      if (kind === "project") {
        return "Deleted project";
      }
      if (kind.includes("provider")) {
        return "Deleted provider";
      }
      return "Deleted";
    case "unresolved":
      return kind;
  }
}

/** Reads the log the scope names. The other scope's query is disabled. */
function useAuditPages(scope: AuditScope, query: AuditQuery) {
  const org = scope === "instance" ? "" : scope.org;
  const orgPages = $api.useInfiniteQuery(
    "get",
    "/api/v1/orgs/{org}/audit-events",
    { params: { path: { org }, query } },
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
      enabled: scope !== "instance",
    },
  );
  const instancePages = $api.useInfiniteQuery(
    "get",
    "/api/v1/admin/audit-events",
    { params: { query } },
    {
      pageParamName: "cursor",
      initialPageParam: null,
      getNextPageParam: (page) => page.next_cursor ?? undefined,
      enabled: scope === "instance",
    },
  );
  return scope === "instance" ? instancePages : orgPages;
}

/**
 * The audit records of one log, newest first, with filters in the URL. The org page and the
 * instance page both render it. A `plan_required` answer on the instance log gets the licence note.
 */
export function AuditLogView({
  scope,
  search,
  onUpdate,
}: {
  scope: AuditScope;
  /** The validated URL search of the route that shows the view. */
  search: AuditSearch;
  /** Writes filter changes to the URL of that route. */
  onUpdate: (patch: Partial<AuditSearch>) => void;
}) {
  // The router keeps the search object's identity while its values are equal, so the query changes
  // only with the URL. `now` is rounded down to the minute, so the query key holds within it.
  const query = useMemo(() => auditQuery(search, floorToMinute(new Date())), [search]);
  const pages = useAuditPages(scope, query);
  const update = onUpdate;

  const records: AuditEvent[] = (pages.data?.pages ?? []).flatMap((page) => page.data);
  const open = search.open === undefined ? undefined : records.find((r) => r.id === search.open);
  const problem = problemOf(pages.error);

  return (
    <div className="flex flex-col gap-4">
      <AuditFilters search={search} onUpdate={update} />
      <div className="flex justify-end">
        <AuditExportMenu scope={scope} query={query} />
      </div>

      {problem?.code === "plan_required" && scope === "instance" ? (
        <p role="note" className="text-body text-muted-foreground">
          The instance audit log needs an enterprise licence that lists audit_log.{" "}
          <Link to="/admin/settings" className="underline">
            See the Settings tab
          </Link>
          .
        </p>
      ) : pages.error ? (
        <p role="alert" className="text-body text-destructive">
          {problemMessage(pages.error, "The audit log could not be loaded")}
        </p>
      ) : pages.isPending ? (
        <Skeleton className="h-64 w-full" />
      ) : records.length === 0 ? (
        <p className="text-body text-muted-foreground">No records match these filters.</p>
      ) : (
        <>
          <Table aria-label="Audit records">
            <TableHeader>
              <TableRow>
                <TableHead>Time</TableHead>
                <TableHead>Actor</TableHead>
                <TableHead>Action</TableHead>
                <TableHead>Target</TableHead>
                <TableHead>IP</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {records.map((record) => (
                <TableRow
                  key={record.id}
                  className="cursor-pointer"
                  onClick={() => {
                    update({ open: record.id });
                  }}
                >
                  <TableCell>
                    <TimeText value={record.at} style="datetime" />
                  </TableCell>
                  <TableCell>
                    <ActorLabel actor={record.actor} />
                  </TableCell>
                  <TableCell>
                    <button
                      type="button"
                      className="font-mono text-left underline-offset-2 hover:underline focus-ring rounded-sm"
                      onClick={(event) => {
                        event.stopPropagation();
                        update({ open: record.id });
                      }}
                    >
                      {record.action}
                    </button>
                    {record.outcome === "failure" ? (
                      <span className="block text-caption text-destructive">failure</span>
                    ) : null}
                  </TableCell>
                  <TableCell>{record.target ? <RefLabel object={record.target} /> : "—"}</TableCell>
                  <TableCell className="font-mono">{record.ip ?? "—"}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          {pages.hasNextPage ? (
            <div className="flex justify-center">
              <Button
                type="button"
                variant="outline"
                disabled={pages.isFetchingNextPage}
                onClick={() => {
                  void pages.fetchNextPage();
                }}
              >
                Load more
              </Button>
            </div>
          ) : null}
        </>
      )}

      <AuditRecordSheet
        record={open}
        onClose={() => {
          update({ open: undefined });
        }}
      />
    </div>
  );
}

function AuditFilters({
  search,
  onUpdate,
}: {
  search: AuditSearch;
  onUpdate: (patch: Partial<AuditSearch>) => void;
}) {
  return (
    <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-5">
      <SelectField
        id="audit-period"
        label="Time"
        value={search.period}
        options={RANGE_OPTIONS}
        disabled={false}
        onChange={(period) => {
          onUpdate({ period });
        }}
      />
      <TextFilter
        id="audit-action"
        label="Action"
        placeholder="member., auth.login"
        value={search.action}
        onCommit={(next) => {
          onUpdate({ action: next });
        }}
      />
      <TextFilter
        id="audit-actor"
        label="Actor ID"
        value={search.actor}
        onCommit={(next) => {
          onUpdate({ actor: next });
        }}
      />
      <TextFilter
        id="audit-target"
        label="Target ID"
        value={search.target}
        onCommit={(next) => {
          onUpdate({ target: next });
        }}
      />
      <SelectField
        id="audit-outcome"
        label="Outcome"
        value={search.outcome ?? "all"}
        options={OUTCOME_OPTIONS}
        disabled={false}
        onChange={(choice) => {
          onUpdate({ outcome: choice === "all" ? undefined : choice });
        }}
      />
    </div>
  );
}

/** A text filter that writes the URL once the typing has paused for 300 ms. */
function TextFilter({
  id,
  label,
  placeholder,
  value,
  onCommit,
}: {
  id: string;
  label: string;
  placeholder?: string;
  value: string | undefined;
  onCommit: (next: string | undefined) => void;
}) {
  const [draft, setDraft] = useState(value ?? "");
  useEffect(() => {
    const timer = window.setTimeout(() => {
      const next = draft.trim() || undefined;
      if (next !== value) {
        onCommit(next);
      }
    }, PAGE_DEBOUNCE_MS);
    return () => {
      window.clearTimeout(timer);
    };
  }, [draft, value, onCommit]);

  return (
    <div className="flex flex-col gap-1.5">
      <Label htmlFor={id}>{label}</Label>
      <Input
        id={id}
        value={draft}
        placeholder={placeholder}
        autoComplete="off"
        onChange={(event) => {
          setDraft(event.target.value);
        }}
      />
    </div>
  );
}

/** The actor: the name and email of a present user, the kind for a system or CLI actor, else the status. */
function ActorLabel({ actor }: { actor: AuditActor }) {
  let label: ReactNode;
  if (actor.status === "present") {
    label = actor.name ? (
      <>
        <span>{actor.name}</span>
        {actor.email ? (
          <span className="block text-caption text-muted-foreground">{actor.email}</span>
        ) : null}
      </>
    ) : (
      <span>{actor.kind}</span>
    );
  } else {
    label = <span>{refStatusLabel(actor.status, actor.kind)}</span>;
  }
  return (
    <span className="flex flex-col">
      {label}
      {actor.id ? <IdText value={actor.id} kind="Actor" /> : null}
    </span>
  );
}

/** A referenced object: its name or email when present, else its status; then its id. */
function RefLabel({ object }: { object: AuditRef }) {
  const label =
    object.status === "present"
      ? (object.name ?? object.email ?? object.kind)
      : refStatusLabel(object.status, object.kind);
  return (
    <span className="flex flex-col">
      <span>{label}</span>
      <IdText value={object.id} kind={object.kind} />
    </span>
  );
}
