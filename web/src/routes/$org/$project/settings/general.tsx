import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { meQuery, projectListOptions, projectQuery } from "@/api/queries";
import { DangerZone } from "@/components/deletions/danger-zone";
import { TextField } from "@/components/form/text-field";
import { LockedFeature } from "@/components/plan/locked-feature";
import { UpgradeButton } from "@/components/plan/upgrade-button";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { ReadOnlyCodeField } from "@/components/settings/read-only-code-field";
import { SettingsFormCard } from "@/components/settings/settings-card";
import { SettingsPage } from "@/components/settings/settings-page";
import { useProject } from "@/components/shell/project-context";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
import { Checkbox } from "@/components/ui/checkbox";
import { Label } from "@/components/ui/label";
import { COMPLETION_TIMEOUT_MAX_S, COMPLETION_TIMEOUT_MIN_S } from "@/lib/completion-timeout";
import { removeProjectReads } from "@/lib/deletions";
import { atLeast } from "@/lib/roles";

export const Route = createFileRoute("/$org/$project/settings/general")({
  component: ProjectGeneralPage,
});

const REASON = "Only project admins can change these settings";
const RETENTION_MAX_DAYS = 36500;

/** Name of the project: the only field this card saves. */
function ProjectNameCard() {
  const { org, project, ref, data } = useProject();
  const canEdit = atLeast(data.role, "admin");
  const queryClient = useQueryClient();
  const mutation = $api.useMutation("patch", "/api/v1/projects/{project}");
  const [name, setName] = useState(data.name);
  const [touched, setTouched] = useState(false);
  const [serverError, setServerError] = useState<string | undefined>();
  const [error, setError] = useState<string | undefined>();
  const [saved, setSaved] = useState(false);

  const trimmed = name.trim();
  const clientError = trimmed.length === 0 ? "Enter a name." : undefined;
  const dirty = trimmed !== data.name;

  function onSubmit() {
    setSaved(false);
    setError(undefined);
    setServerError(undefined);
    mutation.reset();
    if (clientError !== undefined) {
      setTouched(true);
      return;
    }
    mutation.mutate(
      { params: { path: { project: ref } }, body: { name: trimmed } },
      {
        onSuccess: () => {
          setSaved(true);
          void queryClient.invalidateQueries({ queryKey: projectQuery(org, project).queryKey });
          void queryClient.invalidateQueries({ queryKey: projectListOptions(org).queryKey });
        },
        onError: (caught) => {
          const message = fieldErrors(caught).name;
          if (message !== undefined) {
            setServerError(message);
            return;
          }
          if (problemOf(caught)?.status === 403) {
            setError("Only project admins can change these settings.");
            return;
          }
          setError(problemMessage(caught, "Could not save the project."));
        },
      },
    );
  }

  return (
    <SettingsFormCard
      title="Project"
      dirty={dirty}
      invalid={clientError !== undefined}
      pending={mutation.isPending}
      saved={saved}
      error={error}
      disabledReason={canEdit ? undefined : REASON}
      onSubmit={onSubmit}
    >
      <TextField
        id="project-name"
        label="Name"
        value={name}
        required
        maxLength={100}
        error={(touched ? clientError : undefined) ?? serverError}
        disabled={!canEdit || mutation.isPending}
        onChange={(value) => {
          setName(value);
          setTouched(true);
          setSaved(false);
          setServerError(undefined);
        }}
      />
      <ReadOnlyCodeField
        id="project-slug"
        label="Slug"
        value={data.slug}
        description="Part of every URL of this project; cannot be changed."
      />
    </SettingsFormCard>
  );
}

function isRetentionDays(value: string): boolean {
  if (!/^\d+$/.test(value)) {
    return false;
  }
  const days = Number(value);
  return days >= 1 && days <= RETENTION_MAX_DAYS;
}

/** Retention in days, or "keep forever" (`retention_days: null`). Locked without the feature. */
function RetentionCard() {
  const { org, project, ref, data } = useProject();
  const canEdit = atLeast(data.role, "admin");
  const queryClient = useQueryClient();
  const entitlements = useOrgEntitlements(org);
  const locked = entitlements.locked("retention_management");
  const windowDays = entitlements.query.data?.limits.data_access_days;
  const mutation = $api.useMutation("patch", "/api/v1/projects/{project}");
  // Absent and null both keep data forever.
  const stored = data.retention_days ?? null;
  const [forever, setForever] = useState(stored === null);
  const [days, setDays] = useState(stored === null ? "" : String(stored));
  const [touched, setTouched] = useState(false);
  const [serverError, setServerError] = useState<string | undefined>();
  const [error, setError] = useState<string | undefined>();
  const [planRefused, setPlanRefused] = useState(false);
  const [saved, setSaved] = useState(false);

  const clientError =
    !forever && !isRetentionDays(days.trim())
      ? `Enter a whole number of days from 1 to ${RETENTION_MAX_DAYS}.`
      : undefined;
  const next = forever ? null : Number(days.trim());
  const dirty = next !== stored;

  function onSubmit() {
    setSaved(false);
    setError(undefined);
    setPlanRefused(false);
    setServerError(undefined);
    mutation.reset();
    if (clientError !== undefined) {
      setTouched(true);
      return;
    }
    mutation.mutate(
      { params: { path: { project: ref } }, body: { retention_days: next } },
      {
        onSuccess: () => {
          setSaved(true);
          void queryClient.invalidateQueries({ queryKey: projectQuery(org, project).queryKey });
        },
        onError: (caught) => {
          const message = fieldErrors(caught).retention_days;
          if (message !== undefined) {
            setServerError(message);
            return;
          }
          if (problemOf(caught)?.code === "plan_required") {
            setError("Data retention management is part of the Pro plan.");
            setPlanRefused(true);
            return;
          }
          if (problemOf(caught)?.status === 403) {
            setError("Only project admins can change these settings.");
            return;
          }
          setError(problemMessage(caught, "Could not save the retention."));
        },
      },
    );
  }

  const description =
    windowDays === undefined
      ? "Lowering it deletes older data right away."
      : `Lowering it deletes older data right away. Your plan shows the last ${windowDays} days.`;

  return (
    <SettingsFormCard
      title="Data retention"
      dirty={dirty}
      invalid={clientError !== undefined}
      pending={mutation.isPending}
      saved={saved}
      error={error}
      disabledReason={canEdit ? undefined : REASON}
      onSubmit={onSubmit}
      footerExtra={planRefused ? <UpgradeButton url={entitlements.upgradeUrl} /> : undefined}
    >
      {locked ? (
        <LockedFeature feature="retention_management" upgradeUrl={entitlements.upgradeUrl} />
      ) : null}
      <TextField
        id="project-retention"
        type="number"
        label="Retention"
        unit="days"
        inputMode="numeric"
        min={1}
        max={RETENTION_MAX_DAYS}
        step={1}
        value={days}
        error={(touched ? clientError : undefined) ?? serverError}
        description={description}
        disabled={forever || locked || !canEdit || mutation.isPending}
        onChange={(value) => {
          setDays(value);
          setTouched(true);
          setSaved(false);
          setServerError(undefined);
        }}
      />
      <div className="flex items-center gap-2">
        <Checkbox
          id="project-retention-forever"
          checked={forever}
          onCheckedChange={(value) => {
            setForever(value === true);
            setTouched(true);
            setSaved(false);
            setServerError(undefined);
          }}
          disabled={!canEdit || mutation.isPending}
        />
        <Label htmlFor="project-retention-forever">Keep data forever</Label>
      </div>
    </SettingsFormCard>
  );
}

/** How long a trace waits without new spans before online rules evaluate it. */
function TimeoutCard() {
  const { org, project, ref, data } = useProject();
  const canEdit = atLeast(data.role, "admin");
  const queryClient = useQueryClient();
  const mutation = $api.useMutation("patch", "/api/v1/projects/{project}");
  const serverTimeout = data.trace_completion_timeout_s;
  const [timeout, setTimeoutText] = useState(String(serverTimeout));
  // Once the user has typed in the field, later server values no longer overwrite it.
  const [timeoutEdited, setTimeoutEdited] = useState(false);
  const [touched, setTouched] = useState(false);
  const [serverError, setServerError] = useState<string | undefined>();
  const [error, setError] = useState<string | undefined>();
  const [saved, setSaved] = useState(false);
  useEffect(() => {
    if (!timeoutEdited) {
      setTimeoutText(String(serverTimeout));
    }
  }, [serverTimeout, timeoutEdited]);

  const value = timeout.trim();
  const seconds = Number(value);
  const valid =
    /^\d+$/.test(value) &&
    seconds >= COMPLETION_TIMEOUT_MIN_S &&
    seconds <= COMPLETION_TIMEOUT_MAX_S;
  const clientError = valid
    ? undefined
    : `Enter a whole number from ${COMPLETION_TIMEOUT_MIN_S} to ${COMPLETION_TIMEOUT_MAX_S}.`;
  const dirty = value !== String(serverTimeout);

  function onSubmit() {
    setSaved(false);
    setError(undefined);
    setServerError(undefined);
    mutation.reset();
    if (!valid) {
      setTouched(true);
      return;
    }
    mutation.mutate(
      { params: { path: { project: ref } }, body: { trace_completion_timeout_s: seconds } },
      {
        onSuccess: () => {
          setSaved(true);
          void queryClient.invalidateQueries({ queryKey: projectQuery(org, project).queryKey });
        },
        onError: (caught) => {
          const message = fieldErrors(caught).trace_completion_timeout_s;
          if (message !== undefined) {
            setServerError(message);
            return;
          }
          setError(problemMessage(caught, "Could not save the timeout."));
        },
      },
    );
  }

  return (
    <SettingsFormCard
      title="Online evaluation"
      dirty={dirty}
      invalid={!valid}
      pending={mutation.isPending}
      saved={saved}
      error={error}
      disabledReason={canEdit ? undefined : REASON}
      onSubmit={onSubmit}
    >
      <TextField
        id="project-trace-timeout"
        type="number"
        label="Trace completion timeout"
        unit="seconds"
        inputMode="numeric"
        min={COMPLETION_TIMEOUT_MIN_S}
        max={COMPLETION_TIMEOUT_MAX_S}
        step={1}
        value={timeout}
        error={(touched ? clientError : undefined) ?? serverError}
        description={`Wait this long without new spans before evaluating a trace (${COMPLETION_TIMEOUT_MIN_S}–${COMPLETION_TIMEOUT_MAX_S}, default 60).`}
        disabled={!canEdit || mutation.isPending}
        onChange={(next) => {
          setTimeoutEdited(true);
          setTimeoutText(next);
          setTouched(true);
          setSaved(false);
          setServerError(undefined);
        }}
      />
    </SettingsFormCard>
  );
}

function ProjectGeneralPage() {
  const { org, project, ref, data } = useProject();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const me = useQuery(meQuery);
  // Deleting a project is an org action (spec 08): a project-admin override is not enough.
  const orgRole = me.data?.orgs.find((entry) => entry.slug === org)?.role;
  const canDelete = me.isSuccess && atLeast(orgRole, "admin");
  const [dropOpen, setDropOpen] = useState(false);
  const dropMutation = $api.useMutation("delete", "/api/v1/projects/{project}");

  function confirmDrop() {
    dropMutation.mutate(
      { params: { path: { project: ref }, query: { confirm: data.slug } } },
      {
        onSuccess: async () => {
          setDropOpen(false);
          await navigate({ to: "/$org", params: { org }, ignoreBlocker: true });
          // The project layout holds a suspense query on this key, and the 30 s stale time keeps
          // the entry, so it is removed rather than invalidated.
          queryClient.removeQueries({ queryKey: projectQuery(org, project).queryKey });
          removeProjectReads(queryClient, ref);
          void queryClient.invalidateQueries({ queryKey: projectListOptions(org).queryKey });
        },
      },
    );
  }

  const dropError = dropMutation.error
    ? problemOf(dropMutation.error)?.status === 403
      ? "Only organization admins and owners can delete projects."
      : problemMessage(dropMutation.error, "Could not delete the project.")
    : undefined;

  return (
    <SettingsPage title="General" description="Name, URL and data retention of this project.">
      <ProjectNameCard />
      <RetentionCard />
      <TimeoutCard />
      <DangerZone
        title="Delete project"
        description="Deletes every trace, span, score, media file and setting of this project. Data is removed within 24 hours; stored copies are purged after the storage grace period. This cannot be undone."
        buttonLabel="Delete project…"
        disabled={!canDelete}
        disabledReason={
          me.isSuccess && !canDelete
            ? "Only organization admins and owners can delete projects"
            : undefined
        }
        onOpen={() => {
          dropMutation.reset();
          setDropOpen(true);
        }}
      >
        <TypeToConfirmDialog
          open={dropOpen}
          onOpenChange={setDropOpen}
          title={`Delete project ${data.name}?`}
          description="Deletes every trace, span, score, media file and setting of this project. Data is removed within 24 hours; stored copies are purged after the storage grace period. This cannot be undone."
          expected={data.slug}
          inputLabel={`Type the project slug "${data.slug}" to confirm`}
          confirmLabel="Delete project"
          pending={dropMutation.isPending}
          error={dropError}
          onConfirm={confirmDrop}
        />
      </DangerZone>
    </SettingsPage>
  );
}
