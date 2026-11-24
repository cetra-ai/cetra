// SPDX-License-Identifier: Elastic-2.0
import type { ReactNode } from "react";
import type { Schema } from "@/api/types";
import { IdText } from "@/components/id-text";
import { TimeText } from "@/components/time-text";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";

type AuditEvent = Schema<"AuditEvent">;

/** Every field of one audit record. `onClose` runs when the sheet closes. */
export function AuditRecordSheet({
  record,
  onClose,
}: {
  record: AuditEvent | undefined;
  onClose: () => void;
}) {
  return (
    <Sheet
      open={record !== undefined}
      onOpenChange={(open) => {
        if (!open) {
          onClose();
        }
      }}
    >
      <SheetContent className="w-full overflow-y-auto sm:max-w-lg">
        {record ? (
          <>
            <SheetHeader>
              <SheetTitle className="font-mono">{record.action}</SheetTitle>
              <SheetDescription>
                {record.outcome === "failure" ? "The action failed." : "The action succeeded."}
              </SheetDescription>
            </SheetHeader>
            <dl className="grid grid-cols-[minmax(7rem,max-content)_1fr] gap-x-4 gap-y-2 px-4 text-body">
              <Field label="Record">
                <IdText value={record.id} kind="Audit record" />
              </Field>
              <Field label="When">
                <TimeText value={record.at} style="datetime" />
              </Field>
              <Field label="Outcome">{record.outcome}</Field>
              <Field label="Actor kind">{record.actor.kind}</Field>
              <Field label="Actor status">{record.actor.status}</Field>
              {record.actor.id ? (
                <Field label="Actor ID">
                  <IdText value={record.actor.id} kind="Actor" />
                </Field>
              ) : null}
              {record.actor.name ? <Field label="Actor name">{record.actor.name}</Field> : null}
              {record.actor.email ? <Field label="Actor email">{record.actor.email}</Field> : null}
              {record.actor.via ? <Field label="Signed in via">{record.actor.via}</Field> : null}
              {record.actor.credential_id ? (
                <Field label="Credential ID">
                  <IdText value={record.actor.credential_id} kind="Credential" />
                </Field>
              ) : null}
              {record.target ? (
                <>
                  <Field label="Target kind">{record.target.kind}</Field>
                  <Field label="Target status">{record.target.status}</Field>
                  <Field label="Target ID">
                    <IdText value={record.target.id} kind="Target" />
                  </Field>
                  {record.target.name ? (
                    <Field label="Target name">{record.target.name}</Field>
                  ) : null}
                  {record.target.email ? (
                    <Field label="Target email">{record.target.email}</Field>
                  ) : null}
                </>
              ) : null}
              {record.project ? (
                <>
                  <Field label="Project">{record.project.name ?? "Unnamed"}</Field>
                  <Field label="Project ID">
                    <IdText value={record.project.id} kind="Project" />
                  </Field>
                </>
              ) : null}
              {record.ip ? <Field label="IP">{record.ip}</Field> : null}
              {record.user_agent ? <Field label="User agent">{record.user_agent}</Field> : null}
            </dl>
            <div className="flex flex-col gap-1.5 px-4 pb-4">
              <span className="text-body text-muted-foreground">Details</span>
              <pre className="overflow-x-auto rounded-md border bg-muted p-3 font-mono text-caption">
                {JSON.stringify(record.details, null, 2)}
              </pre>
            </div>
          </>
        ) : null}
      </SheetContent>
    </Sheet>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <>
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="min-w-0 break-words">{children}</dd>
    </>
  );
}
