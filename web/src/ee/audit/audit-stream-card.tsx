// SPDX-License-Identifier: Elastic-2.0
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { adminAuditStreamQuery, orgAuditStreamQuery } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { SecretDialog } from "@/components/secret-dialog";
import { SettingsCard } from "@/components/settings/settings-card";
import { UnsavedChangesGuard } from "@/components/settings/unsaved-changes";
import { TimeText } from "@/components/time-text";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import type { AuditScope } from "@/lib/search/audit";

type Stream = Schema<"AuditStream">;

/** The server's stream task runs every 10 s; the status is read again at that pace. */
const STATUS_POLL_MS = 10_000;

const WHOSE = { org: "this organization's", instance: "the instance's" } as const;

const URL_MAX = 2048;
const HEADER_NAME_MAX = 256;
const HEADER_VALUE_MAX = 1024;

const KEEP_VALUE = "Stored; leave blank to keep it";
const NEW_VALUE = "Splunk <token>";

/** What PUT answers: the updated destination, or the created one with its secret. */
type PutAnswer = Schema<"AuditStream"> | Schema<"CreatedAuditStream">;

/** What the card reads of its query. */
type StreamQuery = { data: Stream | undefined; error: unknown; isPending: boolean };

/** A write with no body. `error` is what the route answered, already parsed as a problem. */
type Action<TAnswer> = {
  isPending: boolean;
  error: unknown;
  reset: () => void;
  run: (onDone: (answer: TAnswer) => void) => void;
};

/** PUT, which carries the destination's settings. */
type Save = {
  isPending: boolean;
  error: unknown;
  reset: () => void;
  run: (body: Schema<"PutAuditStream">, onDone: (answer: PutAnswer) => void) => void;
};

/** The four writes of the destination, on one scope's routes. */
type StreamWrites = {
  save: Save;
  remove: Action<undefined>;
  rotate: Action<string>;
  test: Action<Schema<"AuditStreamTest">>;
};

/** The form's fields. The URL starts empty in edit mode too: it is never returned. */
type Draft = { url: string; headerName: string; headerValue: string; enabled: boolean };

const EMPTY_DRAFT: Draft = { url: "", headerName: "", headerValue: "", enabled: true };

const FORM_FIELDS = ["url", "header_name", "header_value"] as const;
type FormField = (typeof FORM_FIELDS)[number];

/**
 * The SIEM destination of one audit log (D-EA-11): owners set the org's on the org's Audit log
 * page, an instance admin sets the instance's on the admin one. The URL, the header value and the
 * signing secret are write-only: the URL is asked for again on every save, and the secret is
 * shown once, at creation and at rotation.
 */
export function AuditStreamCard({ scope }: { scope: AuditScope }) {
  return scope === "instance" ? <InstanceStreamCard /> : <OrgStreamCard org={scope.org} />;
}

function OrgStreamCard({ org }: { org: string }) {
  const query = useQuery({ ...orgAuditStreamQuery(org), refetchInterval: STATUS_POLL_MS });
  const writes = useOrgWrites(org);
  return <StreamCardBody which="org" query={query} writes={writes} />;
}

function InstanceStreamCard() {
  const query = useQuery({ ...adminAuditStreamQuery, refetchInterval: STATUS_POLL_MS });
  const writes = useInstanceWrites();
  return <StreamCardBody which="instance" query={query} writes={writes} />;
}

// Every write carries `gcTime: 0`: the PUT and rotation answers hold a secret and the PUT request
// holds the URL, and neither may stay in the mutation cache (L README, "Secrets and the mutation
// cache"). The card stays mounted, so each success resets its mutation as well.

function useOrgWrites(org: string): StreamWrites {
  const queryClient = useQueryClient();
  const key = orgAuditStreamQuery(org).queryKey;
  const save = $api.useMutation("put", "/api/v1/orgs/{org}/audit-stream", { gcTime: 0 });
  const remove = $api.useMutation("delete", "/api/v1/orgs/{org}/audit-stream", { gcTime: 0 });
  const rotate = $api.useMutation("post", "/api/v1/orgs/{org}/audit-stream/secret", { gcTime: 0 });
  const test = $api.useMutation("post", "/api/v1/orgs/{org}/audit-stream/test", { gcTime: 0 });
  return {
    save: {
      isPending: save.isPending,
      error: save.error,
      reset: () => save.reset(),
      run: (body, onDone) =>
        save.mutate(
          { params: { path: { org } }, body },
          {
            onSuccess: (answer) => {
              queryClient.setQueryData(key, "stream" in answer ? answer.stream : answer);
              save.reset();
              onDone(answer);
            },
          },
        ),
    },
    remove: {
      isPending: remove.isPending,
      error: remove.error,
      reset: () => remove.reset(),
      run: (onDone) =>
        remove.mutate(
          { params: { path: { org } } },
          {
            onSuccess: () => {
              remove.reset();
              void queryClient.invalidateQueries({ queryKey: key });
              onDone(undefined);
            },
          },
        ),
    },
    rotate: {
      isPending: rotate.isPending,
      error: rotate.error,
      reset: () => rotate.reset(),
      run: (onDone) =>
        rotate.mutate(
          { params: { path: { org } } },
          {
            onSuccess: (answer) => {
              rotate.reset();
              onDone(answer.secret);
            },
          },
        ),
    },
    test: {
      isPending: test.isPending,
      error: test.error,
      reset: () => test.reset(),
      run: (onDone) => test.mutate({ params: { path: { org } } }, { onSuccess: onDone }),
    },
  };
}

function useInstanceWrites(): StreamWrites {
  const queryClient = useQueryClient();
  const key = adminAuditStreamQuery.queryKey;
  const save = $api.useMutation("put", "/api/v1/admin/audit-stream", { gcTime: 0 });
  const remove = $api.useMutation("delete", "/api/v1/admin/audit-stream", { gcTime: 0 });
  const rotate = $api.useMutation("post", "/api/v1/admin/audit-stream/secret", { gcTime: 0 });
  const test = $api.useMutation("post", "/api/v1/admin/audit-stream/test", { gcTime: 0 });
  return {
    save: {
      isPending: save.isPending,
      error: save.error,
      reset: () => save.reset(),
      run: (body, onDone) =>
        save.mutate(
          { body },
          {
            onSuccess: (answer) => {
              queryClient.setQueryData(key, "stream" in answer ? answer.stream : answer);
              save.reset();
              onDone(answer);
            },
          },
        ),
    },
    remove: {
      isPending: remove.isPending,
      error: remove.error,
      reset: () => remove.reset(),
      run: (onDone) =>
        remove.mutate(
          { params: {} },
          {
            onSuccess: () => {
              remove.reset();
              void queryClient.invalidateQueries({ queryKey: key });
              onDone(undefined);
            },
          },
        ),
    },
    rotate: {
      isPending: rotate.isPending,
      error: rotate.error,
      reset: () => rotate.reset(),
      run: (onDone) =>
        rotate.mutate(
          { params: {} },
          {
            onSuccess: (answer) => {
              rotate.reset();
              onDone(answer.secret);
            },
          },
        ),
    },
    test: {
      isPending: test.isPending,
      error: test.error,
      reset: () => test.reset(),
      run: (onDone) => test.mutate({ params: {} }, { onSuccess: onDone }),
    },
  };
}

function StreamCardBody({
  which,
  query,
  writes,
}: {
  which: keyof typeof WHOSE;
  query: StreamQuery;
  writes: StreamWrites;
}) {
  // A failed refetch keeps the previous data, so "no destination" is read from the 404 itself.
  const problem = problemOf(query.error);
  const missing = problem?.status === 404;
  const stream = missing ? undefined : query.data;
  const [shown, setShown] = useState<{ title: string; secret: string } | undefined>();
  const [removing, setRemoving] = useState(false);
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState<Draft>(EMPTY_DRAFT);
  const [touched, setTouched] = useState<ReadonlySet<FormField>>(() => new Set());
  const [testAnswer, setTestAnswer] = useState<Schema<"AuditStreamTest"> | undefined>();

  const busy =
    writes.save.isPending ||
    writes.remove.isPending ||
    writes.rotate.isPending ||
    writes.test.isPending;
  const description = `Send ${WHOSE[which]} audit records to an HTTPS endpoint (Splunk, Datadog or any NDJSON receiver) in signed batches, oldest first, at least once.`;

  const name = draft.headerName.trim();
  const value = draft.headerValue;
  const clientErrors: Record<string, string> = {};
  if (draft.url.trim() === "") {
    clientErrors.url = "Enter a URL.";
  }
  if (name === "" && value.trim() !== "") {
    clientErrors.header_name = "Enter the header's name.";
  }
  // A blank value keeps the stored one only under the stored name (plan P-2).
  if (name !== "" && value.trim() === "" && stream?.header_name !== name) {
    clientErrors.header_value = "Enter the header's value.";
  }
  const invalid = Object.keys(clientErrors).length > 0;
  const dirty =
    draft.url.trim() !== "" ||
    draft.headerName !== (stream?.header_name ?? "") ||
    draft.headerValue !== "" ||
    draft.enabled !== (stream?.enabled ?? true);
  // Without a destination the form is the card; with one, it replaces the view while editing.
  const formShown = editing || stream === undefined;

  // A server `400` names a field, which shows it under the field; anything else is the form's.
  const serverErrors = fieldErrors(writes.save.error);
  const fieldHasServerError = FORM_FIELDS.some((field) => serverErrors[field] !== undefined);
  const saveError =
    !fieldHasServerError && writes.save.error !== null && writes.save.error !== undefined
      ? problemMessage(writes.save.error, "Could not save the audit stream.")
      : undefined;
  const testError = writes.test.error
    ? problemMessage(writes.test.error, "Could not send the test line.")
    : undefined;
  const rotateError = writes.rotate.error
    ? problemMessage(writes.rotate.error, "Could not rotate the secret.")
    : undefined;

  if (query.isPending) {
    return <Skeleton className="h-24 w-full" />;
  }
  // A lapsed licence (D-EA-19): the page shows its licence note instead of the log, and the card
  // with it. Nothing is managed here; removing the destination stays possible through the API.
  if (problem?.code === "plan_required") {
    return null;
  }
  if (stream === undefined && !missing) {
    return (
      <SettingsCard title="Streaming" description={description}>
        <FormError message={problemMessage(query.error, "Could not load the audit stream.")} />
      </SettingsCard>
    );
  }

  /** The form with the URL asked for again; `enabled` is the value the switch was flipped to. */
  function startEdit(enabled: boolean) {
    writes.save.reset();
    setTouched(new Set());
    setTestAnswer(undefined);
    setDraft({ url: "", headerName: stream?.header_name ?? "", headerValue: "", enabled });
    setEditing(true);
  }

  function errorOf(field: FormField): string | undefined {
    // In edit mode the URL is empty until it is typed, and Save is disabled until then: show why.
    const reveal = touched.has(field) || (stream !== undefined && field === "url");
    return serverErrors[field] ?? (reveal ? clientErrors[field] : undefined);
  }

  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    writes.save.reset();
    if (invalid) {
      setTouched(new Set(FORM_FIELDS));
      return;
    }
    const body: Schema<"PutAuditStream"> = { url: draft.url.trim(), enabled: draft.enabled };
    if (name !== "") {
      body.header_name = name;
    }
    if (value !== "") {
      body.header_value = value;
    }
    writes.save.run(body, (answer) => {
      setEditing(false);
      setTouched(new Set());
      setTestAnswer(undefined);
      if ("secret" in answer) {
        setShown({ title: "Signing secret", secret: answer.secret });
      }
    });
  }

  return (
    <SettingsCard title="Streaming" description={description}>
      {formShown ? (
        <form
          noValidate
          aria-label={
            stream === undefined ? "Add the stream destination" : "Edit the stream destination"
          }
          className="flex flex-col gap-4"
          onSubmit={submit}
        >
          <TextField
            id="audit-stream-url"
            label="URL"
            value={draft.url}
            autoComplete="off"
            maxLength={URL_MAX}
            placeholder="https://siem.example.com:8088/services/collector/raw"
            description={
              stream === undefined
                ? "The endpoint cetra posts the batches to. Stored sealed, never shown again."
                : "Enter the endpoint's URL again: it is stored sealed and never shown."
            }
            error={errorOf("url")}
            disabled={writes.save.isPending}
            onChange={(url) => {
              setDraft({ ...draft, url });
              setTouched((current) => new Set(current).add("url"));
            }}
          />
          <div className="grid gap-4 sm:grid-cols-2">
            <TextField
              id="audit-stream-header-name"
              label="Header name"
              value={draft.headerName}
              maxLength={HEADER_NAME_MAX}
              placeholder="Authorization"
              description="Optional: one header sent with every batch."
              error={errorOf("header_name")}
              disabled={writes.save.isPending}
              onChange={(headerName) => {
                setDraft({ ...draft, headerName });
                setTouched((current) => new Set(current).add("header_name"));
              }}
            />
            <TextField
              id="audit-stream-header-value"
              label="Header value"
              type="password"
              autoComplete="new-password"
              value={draft.headerValue}
              maxLength={HEADER_VALUE_MAX}
              placeholder={
                stream !== undefined && stream.header_name === name ? KEEP_VALUE : NEW_VALUE
              }
              error={errorOf("header_value")}
              disabled={writes.save.isPending}
              onChange={(headerValue) => {
                setDraft({ ...draft, headerValue });
                setTouched((current) => new Set(current).add("header_value"));
              }}
            />
          </div>
          <div className="flex flex-col gap-2">
            <div className="flex items-center gap-2">
              <Switch
                id="audit-stream-enabled"
                checked={draft.enabled}
                disabled={writes.save.isPending}
                onCheckedChange={(enabled) => {
                  setDraft({ ...draft, enabled });
                }}
              />
              <Label htmlFor="audit-stream-enabled">Enabled</Label>
            </div>
            <p className="text-caption text-muted-foreground">
              While it is off nothing is sent. Turned on again, the stream resumes from the last
              record the endpoint accepted.
            </p>
          </div>
          <FormError message={saveError} />
          <div className="flex items-center justify-end gap-2">
            {stream !== undefined ? (
              <Button
                type="button"
                variant="outline"
                disabled={writes.save.isPending}
                onClick={() => {
                  writes.save.reset();
                  setEditing(false);
                  setTouched(new Set());
                  // Back to the view's values, so nothing reads as unsaved.
                  setDraft({
                    url: "",
                    headerName: stream.header_name ?? "",
                    headerValue: "",
                    enabled: stream.enabled,
                  });
                }}
              >
                Cancel
              </Button>
            ) : null}
            <SubmitButton pending={writes.save.isPending} disabled={!dirty || invalid}>
              Save
            </SubmitButton>
          </div>
        </form>
      ) : (
        <>
          <div className="flex flex-col gap-2">
            <div className="flex items-center gap-2">
              <Switch
                id="audit-stream-enabled"
                checked={stream.enabled}
                disabled={busy}
                onCheckedChange={(enabled) => {
                  startEdit(enabled);
                }}
              />
              <Label htmlFor="audit-stream-enabled">Enabled</Label>
            </div>
            <p className="text-caption text-muted-foreground">
              {stream.enabled
                ? `New records go to ${stream.url_host} about every 10 seconds.`
                : "Nothing is sent while it is off. Turn it on to resume from the last record the endpoint accepted."}{" "}
              Changing it asks for the URL again: cetra never shows it.
            </p>
          </div>
          <dl className="grid grid-cols-[minmax(6rem,max-content)_1fr] gap-x-6 gap-y-2 text-body">
            <dt className="text-muted-foreground">Host</dt>
            <dd className="font-mono">{stream.url_host}</dd>
            {stream.header_name ? (
              <>
                <dt className="text-muted-foreground">Header</dt>
                <dd className="font-mono">{stream.header_name}</dd>
              </>
            ) : null}
          </dl>
          <div className="flex flex-col gap-1 text-body">
            <p>
              {stream.status.last_success_at ? (
                <>
                  Last delivered <TimeText value={stream.status.last_success_at} style="datetime" />
                </>
              ) : (
                "Never delivered"
              )}
            </p>
            <p>
              {stream.status.lag} {stream.status.lag === 1 ? "record" : "records"} behind
            </p>
            {stream.status.last_error ? (
              <p className="text-destructive">
                {stream.status.last_error}
                {stream.status.failing_since ? (
                  <>
                    {" "}
                    (failing since <TimeText value={stream.status.failing_since} style="datetime" />
                    )
                  </>
                ) : null}
              </p>
            ) : null}
            {stream.status.lost_records ? (
              <p className="text-amber-700 dark:text-amber-400">
                Some records were deleted by retention before they were sent.
              </p>
            ) : null}
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <Button
              type="button"
              variant="outline"
              disabled={busy}
              onClick={() => {
                startEdit(stream.enabled);
              }}
            >
              Edit
            </Button>
            <Button
              type="button"
              variant="outline"
              disabled={busy}
              onClick={() => {
                writes.test.reset();
                setTestAnswer(undefined);
                writes.test.run((answer) => {
                  setTestAnswer(answer);
                });
              }}
            >
              Send test
            </Button>
            <Button
              type="button"
              variant="outline"
              disabled={busy}
              onClick={() => {
                writes.rotate.reset();
                writes.rotate.run((secret) => {
                  setShown({ title: "New signing secret", secret });
                });
              }}
            >
              Rotate secret
            </Button>
            <Button
              type="button"
              variant="destructive"
              disabled={busy}
              onClick={() => {
                writes.remove.reset();
                setRemoving(true);
              }}
            >
              Remove
            </Button>
          </div>
          {testAnswer ? (
            <p role="status" className="text-body">
              {testAnswer.reached
                ? testAnswer.status === undefined
                  ? "Reached"
                  : `Reached, status ${testAnswer.status}`
                : "Not reached"}
            </p>
          ) : null}
          <FormError message={testError ?? rotateError} />
        </>
      )}
      <UnsavedChangesGuard
        dirty={formShown && dirty && !writes.save.isPending}
        ignoreSearchChanges
      />
      <SecretDialog
        open={shown !== undefined}
        title={shown?.title ?? ""}
        secret={shown?.secret ?? ""}
        description="Use it to verify the X-Cetra-Signature header."
        onDone={() => {
          setShown(undefined);
        }}
      />
      {stream !== undefined ? (
        <TypeToConfirmDialog
          open={removing}
          onOpenChange={(next) => {
            if (!next) {
              setRemoving(false);
              writes.remove.reset();
            }
          }}
          title="Remove the audit stream?"
          description={`Records stop being sent to ${stream.url_host}. Setting a destination up again starts from the newest record: what was recorded in between is not sent.`}
          expected="remove"
          inputLabel='Type "remove" to confirm'
          confirmLabel="Remove"
          pending={writes.remove.isPending}
          error={
            writes.remove.error
              ? problemMessage(writes.remove.error, "Could not remove the audit stream.")
              : undefined
          }
          onConfirm={() => {
            writes.remove.run(() => {
              setRemoving(false);
              setTestAnswer(undefined);
              setEditing(false);
              setTouched(new Set());
              setDraft(EMPTY_DRAFT);
            });
          }}
        />
      ) : null}
    </SettingsCard>
  );
}
