import { useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { channelsQuery } from "@/api/queries-alerts";
import type { Schema } from "@/api/types";
import { type HeaderRow, HeadersEditor } from "@/components/alerts/headers-editor";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { UnsavedChangesGuard } from "@/components/settings/unsaved-changes";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Textarea } from "@/components/ui/textarea";
import { channelKindLabel } from "@/lib/alerts";

export const CANNOT_MANAGE_CHANNELS = "Only project members and above can manage channels";

const KINDS: readonly Schema<"ChannelKind">[] = ["slack", "webhook", "email"];
/** The form fields a server `400` can name; other fields are not shown. */
const FORM_FIELDS = ["name", "kind", "url", "secret", "headers", "recipients"] as const;

// Module-level so every header row of every open form gets a distinct React key.
let headerRowCounter = 0;
function nextHeaderRowId(): number {
  headerRowCounter += 1;
  return headerRowCounter;
}

/** Trimmed names; a later duplicate name wins. Rows with an empty name are dropped. */
function headerMap(rows: HeaderRow[]): Record<string, string> {
  return Object.fromEntries(
    rows.filter((row) => row.name.trim() !== "").map((row) => [row.name.trim(), row.value]),
  );
}

function recipientList(text: string): string[] {
  return text
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line !== "");
}

// The response of each holds a secret or a URL: gcTime 0 drops it from the mutation cache once
// reset (L README, "Secrets and the mutation cache").
function useCreateChannel() {
  return $api.useMutation("post", "/api/v1/projects/{project}/channels", { gcTime: 0 });
}

function useUpdateChannel() {
  return $api.useMutation("patch", "/api/v1/projects/{project}/channels/{id}", { gcTime: 0 });
}

/**
 * The create or edit form of a channel, shown in the settings panel. Errors show once a field
 * was touched (or after a server refusal); Save waits for a valid, changed form.
 */
export function ChannelForm({
  org,
  project,
  channel,
  canEdit,
  onDone,
}: {
  org: string;
  project: string;
  /** Absent: create. Present: edit this channel. */
  channel?: Schema<"NotificationChannel">;
  canEdit: boolean;
  /** Called after a save. The panel closes. */
  onDone: () => void;
}) {
  const queryClient = useQueryClient();
  const create = useCreateChannel();
  const update = useUpdateChannel();
  const pending = create.isPending || update.isPending;
  const locked = pending || !canEdit;
  const [name, setName] = useState(channel?.name ?? "");
  const [kind, setKind] = useState<Schema<"ChannelKind">>("slack");
  const [url, setUrl] = useState("");
  const [secret, setSecret] = useState("");
  // Edit mode: one row per header name, values unknown (the API never returns them).
  const [headers, setHeaders] = useState<HeaderRow[]>(() =>
    (channel?.header_names ?? []).map((header) => ({
      id: nextHeaderRowId(),
      name: header,
      value: "",
    })),
  );
  const [headersTouched, setHeadersTouched] = useState(false);
  const [recipients, setRecipients] = useState(channel?.recipients.join("\n") ?? "");
  const [touched, setTouched] = useState<ReadonlySet<string>>(() => new Set());
  const [serverErrors, setServerErrors] = useState<Record<string, string>>({});
  const [formError, setFormError] = useState<string | undefined>();

  // In edit mode the kind is fixed (D-K-6).
  const activeKind = channel?.kind ?? kind;
  const trimmedName = name.trim();
  const recipientValues = recipientList(recipients);

  const clientErrors: Record<string, string> = {};
  if (!trimmedName) {
    clientErrors.name = "Enter a name.";
  }
  if (!channel) {
    if ((activeKind === "slack" || activeKind === "webhook") && !url.trim()) {
      clientErrors.url = "Enter a URL.";
    }
    if (activeKind === "webhook" && !secret.trim()) {
      clientErrors.secret = "Enter a signing secret.";
    }
    if (activeKind === "email" && recipientValues.length === 0) {
      clientErrors.recipients = "Enter at least one address.";
    }
  }
  const invalid = Object.keys(clientErrors).length > 0;

  const dirty = channel
    ? trimmedName !== channel.name ||
      url.trim() !== "" ||
      secret.trim() !== "" ||
      headersTouched ||
      JSON.stringify(recipientValues) !== JSON.stringify(channel.recipients)
    : name !== "" || url !== "" || secret !== "" || recipients.trim() !== "" || headers.length > 0;

  function touch(field: string) {
    setTouched((current) => (current.has(field) ? current : new Set(current).add(field)));
  }

  function errorOf(field: string): string | undefined {
    return serverErrors[field] ?? (touched.has(field) ? clientErrors[field] : undefined);
  }

  function invalidate() {
    void queryClient.invalidateQueries({ queryKey: channelsQuery(org, project).queryKey });
  }

  function showError(caught: unknown) {
    const all = fieldErrors(caught);
    const mapped: Record<string, string> = {};
    for (const field of FORM_FIELDS) {
      if (all[field] !== undefined) {
        mapped[field] = all[field];
      }
    }
    setServerErrors(mapped);
    if (Object.keys(mapped).length > 0) {
      setFormError(undefined);
      return;
    }
    const status = problemOf(caught)?.status;
    if (status === 409) {
      setFormError(problemMessage(caught, "A channel with this name already exists."));
    } else if (status === 403) {
      setFormError("Only project members and above can manage channels.");
    } else {
      setFormError(problemMessage(caught, "Could not save the channel."));
    }
  }

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    setServerErrors({});
    setFormError(undefined);
    if (invalid) {
      setTouched(new Set(["name", "url", "secret", "recipients"]));
      return;
    }
    const trimmedUrl = url.trim();
    const trimmedSecret = secret.trim();

    const path = { project: projectRef(org, project) };
    if (!channel) {
      const body: Schema<"CreateChannelRequest"> = { name: trimmedName, kind: activeKind };
      if (activeKind === "slack") {
        body.url = trimmedUrl;
      }
      if (activeKind === "webhook") {
        body.url = trimmedUrl;
        body.secret = trimmedSecret;
        body.headers = headerMap(headers);
      }
      if (activeKind === "email") {
        body.recipients = recipientValues;
      }
      create.mutate(
        { params: { path }, body },
        {
          onSuccess: () => {
            invalidate();
            onDone();
          },
          onError: showError,
        },
      );
      return;
    }

    const body: Schema<"UpdateChannelRequest"> = {};
    if (trimmedName !== channel.name) {
      body.name = trimmedName;
    }
    if ((activeKind === "slack" || activeKind === "webhook") && trimmedUrl) {
      body.url = trimmedUrl;
    }
    if (activeKind === "webhook") {
      if (trimmedSecret) {
        body.secret = trimmedSecret;
      }
      if (headersTouched) {
        body.headers = headerMap(headers);
      }
    }
    if (
      activeKind === "email" &&
      JSON.stringify(recipientValues) !== JSON.stringify(channel.recipients)
    ) {
      body.recipients = recipientValues;
    }
    if (Object.keys(body).length === 0) {
      onDone();
      return;
    }
    update.mutate(
      { params: { path: { ...path, id: channel.id } }, body },
      {
        onSuccess: () => {
          invalidate();
          onDone();
        },
        onError: showError,
      },
    );
  }

  const urlDescription = channel
    ? activeKind === "slack"
      ? `Leave empty to keep the current URL (${channel.target_hint}).`
      : "Leave empty to keep the current one."
    : activeKind === "slack"
      ? "The Slack incoming webhook URL (https://hooks.slack.com/…)."
      : undefined;

  return (
    <>
      <form
        noValidate
        aria-label={channel ? "Edit channel" : "New channel"}
        className="flex flex-col gap-4"
        onSubmit={onSubmit}
      >
        <TextField
          id="channel-name"
          label="Name"
          value={name}
          maxLength={100}
          error={errorOf("name")}
          disabled={locked}
          onChange={(value) => {
            setName(value);
            touch("name");
          }}
        />
        {channel ? (
          <p className="text-body">Kind: {channelKindLabel(channel.kind)}</p>
        ) : (
          <div className="flex flex-col gap-2">
            <Label id="channel-kind-label">Kind</Label>
            <RadioGroup
              aria-labelledby="channel-kind-label"
              value={kind}
              onValueChange={(value) => {
                const next = KINDS.find((item) => item === value);
                if (next) {
                  setKind(next);
                }
              }}
              disabled={locked}
            >
              {KINDS.map((item) => (
                <div key={item} className="flex items-center gap-2">
                  <RadioGroupItem value={item} id={`channel-kind-${item}`} />
                  <Label htmlFor={`channel-kind-${item}`}>{channelKindLabel(item)}</Label>
                </div>
              ))}
            </RadioGroup>
          </div>
        )}
        {activeKind === "slack" ? (
          <TextField
            id="channel-url"
            label="Webhook URL"
            value={url}
            autoComplete="off"
            description={urlDescription}
            error={errorOf("url")}
            disabled={locked}
            onChange={(value) => {
              setUrl(value);
              touch("url");
            }}
          />
        ) : null}
        {activeKind === "webhook" ? (
          <>
            <TextField
              id="channel-url"
              label="URL"
              value={url}
              autoComplete="off"
              description={urlDescription}
              error={errorOf("url")}
              disabled={locked}
              onChange={(value) => {
                setUrl(value);
                touch("url");
              }}
            />
            <TextField
              id="channel-secret"
              label="Signing secret"
              type="password"
              autoComplete="new-password"
              value={secret}
              description={
                channel
                  ? "Leave empty to keep the current one."
                  : "16 to 256 characters. Verify the X-Cetra-Signature header with it."
              }
              error={errorOf("secret")}
              disabled={locked}
              onChange={(value) => {
                setSecret(value);
                touch("secret");
              }}
            />
            <HeadersEditor
              rows={headers}
              nextId={nextHeaderRowId}
              description={
                channel
                  ? "Values are not shown. Saving replaces every header; re-enter the values."
                  : undefined
              }
              error={errorOf("headers")}
              disabled={locked}
              onChange={(rows) => {
                setHeaders(rows);
                setHeadersTouched(true);
              }}
            />
          </>
        ) : null}
        {activeKind === "email" ? (
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="channel-recipients">Recipients</Label>
            <Textarea
              id="channel-recipients"
              name="channel-recipients"
              value={recipients}
              disabled={locked}
              aria-invalid={errorOf("recipients") ? true : undefined}
              aria-describedby={
                errorOf("recipients")
                  ? "channel-recipients-description channel-recipients-error"
                  : "channel-recipients-description"
              }
              onChange={(event) => {
                setRecipients(event.target.value);
                touch("recipients");
              }}
            />
            <p id="channel-recipients-description" className="text-caption text-muted-foreground">
              One address per line, at most 20.
            </p>
            {errorOf("recipients") ? (
              <p id="channel-recipients-error" className="text-caption text-destructive">
                {errorOf("recipients")}
              </p>
            ) : null}
            {!channel ? (
              <p className="text-caption text-muted-foreground">
                Email needs SMTP on the server. Without it the channel cannot be created.
              </p>
            ) : null}
          </div>
        ) : null}
        <FormError message={formError} />
        <div className="flex items-center justify-end gap-3">
          <div className="mr-auto">
            {dirty ? (
              <span className="text-caption text-muted-foreground">Unsaved changes</span>
            ) : null}
          </div>
          <DisabledReason reason={canEdit ? undefined : CANNOT_MANAGE_CHANNELS}>
            <SubmitButton pending={pending} disabled={!dirty || invalid || !canEdit}>
              {channel ? "Save" : "Create channel"}
            </SubmitButton>
          </DisabledReason>
        </div>
      </form>
      <UnsavedChangesGuard dirty={dirty && !pending} />
    </>
  );
}
