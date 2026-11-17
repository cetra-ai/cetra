import { useQuery, useQueryClient } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { type FormEvent, useEffect, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { meQuery, projectListOptions, projectQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { DangerZone } from "@/components/deletions/danger-zone";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { FormSuccess } from "@/components/form/form-success";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { LockedFeature } from "@/components/plan/locked-feature";
import { UpgradeButton } from "@/components/plan/upgrade-button";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { SettingsPage } from "@/components/settings/settings-page";
import { useProject } from "@/components/shell/project-context";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { COMPLETION_TIMEOUT_MAX_S, COMPLETION_TIMEOUT_MIN_S } from "@/lib/completion-timeout";
import { removeProjectReads } from "@/lib/deletions";
import { atLeast } from "@/lib/roles";

export const Route = createFileRoute("/$org/$project/settings/general")({
  component: ProjectGeneralPage,
});

type ProjectPatch = Schema<"UpdateProjectRequest">;

/**
 * `retention_days: null` keeps data forever. `undefined` leaves retention alone: a rename must
 * not send it.
 */
function projectPatch(name: string, retentionDays: number | null | undefined): ProjectPatch {
  if (retentionDays === undefined) {
    return { name };
  }
  return { name, retention_days: retentionDays };
}

function ProjectGeneralPage() {
  const { org, project, ref, data } = useProject();
  const canEdit = atLeast(data.role, "admin");
  const reason = "Only project admins can change these settings";
  const entitlements = useOrgEntitlements(org);
  const retentionLocked = entitlements.locked("retention_management");
  const windowDays = entitlements.query.data?.limits.data_access_days;
  const [planRefused, setPlanRefused] = useState(false);
  const [name, setName] = useState(data.name);
  const [forever, setForever] = useState(data.retention_days === undefined);
  const [days, setDays] = useState(
    data.retention_days === undefined ? "" : String(data.retention_days),
  );
  const [nameError, setNameError] = useState<string | undefined>();
  const [daysError, setDaysError] = useState<string | undefined>();
  const [error, setError] = useState<string | undefined>();
  const [saved, setSaved] = useState(false);
  const mutation = $api.useMutation("patch", "/api/v1/projects/{project}");
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const me = useQuery(meQuery);
  // Deleting a project is an org action (spec 08): a project-admin override is not enough.
  const orgRole = me.data?.orgs.find((entry) => entry.slug === org)?.role;
  const canDelete = me.isSuccess && atLeast(orgRole, "admin");
  const [dropOpen, setDropOpen] = useState(false);
  const dropMutation = $api.useMutation("delete", "/api/v1/projects/{project}");
  const serverTimeout = data.trace_completion_timeout_s;
  const [timeout, setTimeoutText] = useState(String(serverTimeout));
  // Once the user has typed in the field, later server values no longer overwrite it.
  const [timeoutEdited, setTimeoutEdited] = useState(false);
  const [timeoutError, setTimeoutError] = useState<string | undefined>();
  const [timeoutSaved, setTimeoutSaved] = useState(false);
  const [timeoutFormError, setTimeoutFormError] = useState<string | undefined>();
  const timeoutMutation = $api.useMutation("patch", "/api/v1/projects/{project}");
  useEffect(() => {
    if (!timeoutEdited) {
      setTimeoutText(String(serverTimeout));
    }
  }, [serverTimeout, timeoutEdited]);

  function onTimeoutSubmit(event: FormEvent) {
    event.preventDefault();
    setTimeoutSaved(false);
    setTimeoutFormError(undefined);
    timeoutMutation.reset();
    const value = timeout.trim();
    const seconds = Number(value);
    if (
      !/^\d+$/.test(value) ||
      seconds < COMPLETION_TIMEOUT_MIN_S ||
      seconds > COMPLETION_TIMEOUT_MAX_S
    ) {
      setTimeoutError(
        `Enter a whole number from ${COMPLETION_TIMEOUT_MIN_S} to ${COMPLETION_TIMEOUT_MAX_S}.`,
      );
      return;
    }
    setTimeoutError(undefined);
    timeoutMutation.mutate(
      { params: { path: { project: ref } }, body: { trace_completion_timeout_s: seconds } },
      {
        onSuccess: () => {
          setTimeoutSaved(true);
          void queryClient.invalidateQueries({ queryKey: projectQuery(org, project).queryKey });
        },
        onError: (caught) => {
          const fields = fieldErrors(caught);
          setTimeoutError(fields.trace_completion_timeout_s);
          if (fields.trace_completion_timeout_s === undefined) {
            setTimeoutFormError(problemMessage(caught, "Could not save the timeout."));
          }
        },
      },
    );
  }

  /** The retention to send: nothing when it is unchanged, or locked and not being cleared. */
  function retentionChange(next: number | null): number | null | undefined {
    const stored = data.retention_days ?? null;
    if (next === stored || (retentionLocked && next !== null)) {
      return undefined;
    }
    return next;
  }

  function onSubmit(event: FormEvent) {
    event.preventDefault();
    setSaved(false);
    setError(undefined);
    setPlanRefused(false);
    setNameError(undefined);
    setDaysError(undefined);
    mutation.reset();
    const trimmed = name.trim();
    let nextNameError: string | undefined;
    let nextDaysError: string | undefined;
    if (trimmed.length === 0) {
      nextNameError = "Enter a name.";
    }
    if (!forever) {
      const value = days.trim();
      if (!/^\d+$/.test(value) || Number(value) < 1 || Number(value) > 36500) {
        nextDaysError = "Enter a whole number of days from 1 to 36500.";
      }
    }
    setNameError(nextNameError);
    setDaysError(nextDaysError);
    if (nextNameError !== undefined || nextDaysError !== undefined) {
      return;
    }
    mutation.mutate(
      {
        params: { path: { project: ref } },
        body: projectPatch(trimmed, retentionChange(forever ? null : Number(days.trim()))),
      },
      {
        onSuccess: () => {
          setSaved(true);
          void queryClient.invalidateQueries({ queryKey: projectQuery(org, project).queryKey });
          void queryClient.invalidateQueries({ queryKey: projectListOptions(org).queryKey });
        },
        onError: (caught) => {
          const fields = fieldErrors(caught);
          setNameError(fields.name);
          setDaysError(fields.retention_days);
          if (fields.name !== undefined || fields.retention_days !== undefined) {
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
          setError(problemMessage(caught, "Could not save the project."));
        },
      },
    );
  }

  function confirmDrop() {
    dropMutation.mutate(
      { params: { path: { project: ref }, query: { confirm: data.slug } } },
      {
        onSuccess: async () => {
          setDropOpen(false);
          await navigate({ to: "/$org", params: { org } });
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
      <form
        noValidate
        aria-label="Project"
        className="flex max-w-md flex-col gap-4"
        onSubmit={onSubmit}
      >
        <TextField
          id="project-name"
          label="Name"
          value={name}
          required
          maxLength={100}
          error={nameError}
          disabled={!canEdit || mutation.isPending}
          onChange={(value) => {
            setName(value);
            setSaved(false);
          }}
        />
        <TextField
          id="project-slug"
          label="Slug"
          value={data.slug}
          readOnly
          onChange={() => {}}
          description="The slug is part of every URL of this project and cannot be changed."
        />
        {retentionLocked ? (
          <LockedFeature feature="retention_management" upgradeUrl={entitlements.upgradeUrl} />
        ) : null}
        <TextField
          id="project-retention"
          type="number"
          label="Retention (days)"
          value={days}
          error={daysError}
          disabled={forever || retentionLocked || !canEdit || mutation.isPending}
          description={
            windowDays === undefined
              ? "Lowering retention drops older data right away. Deleted data cannot be recovered. Evaluation results and scores are kept by the day they were written, not by the day of the trace they score."
              : `Lowering retention drops older data right away. Deleted data cannot be recovered. Evaluation results and scores are kept by the day they were written, not by the day of the trace they score. Your plan shows the last ${windowDays} days; older data is hidden, then deleted 30 days later.`
          }
          onChange={(value) => {
            setDays(value);
            setSaved(false);
          }}
        />
        <div className="flex items-center gap-2">
          <Checkbox
            id="project-retention-forever"
            checked={forever}
            onCheckedChange={(value) => {
              setForever(value === true);
              setSaved(false);
            }}
            disabled={!canEdit || mutation.isPending}
          />
          <Label htmlFor="project-retention-forever">Keep data forever</Label>
        </div>
        <FormError message={error} />
        {planRefused ? <UpgradeButton url={entitlements.upgradeUrl} /> : null}
        <FormSuccess message={saved ? "Saved" : undefined} />
        <DisabledReason reason={canEdit ? undefined : reason}>
          <SubmitButton pending={mutation.isPending} disabled={!canEdit}>
            Save
          </SubmitButton>
        </DisabledReason>
      </form>
      <section className="flex max-w-md flex-col gap-4">
        <h2 className="text-body leading-none font-medium">Online evaluation</h2>
        <form
          noValidate
          aria-label="Online evaluation"
          className="flex flex-col gap-4"
          onSubmit={onTimeoutSubmit}
        >
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="project-trace-timeout">Trace completion timeout (seconds)</Label>
            <Input
              id="project-trace-timeout"
              name="project-trace-timeout"
              type="number"
              min={COMPLETION_TIMEOUT_MIN_S}
              max={COMPLETION_TIMEOUT_MAX_S}
              step={1}
              value={timeout}
              aria-invalid={timeoutError ? true : undefined}
              aria-describedby="project-trace-timeout-description"
              disabled={!canEdit || timeoutMutation.isPending}
              onChange={(event) => {
                setTimeoutEdited(true);
                setTimeoutText(event.target.value);
                setTimeoutSaved(false);
              }}
            />
            <p
              id="project-trace-timeout-description"
              className="text-caption text-muted-foreground"
            >
              Online rules evaluate a trace when its root span has ended and 5 seconds pass without
              a new span, or after this many seconds without a new span. {COMPLETION_TIMEOUT_MIN_S}{" "}
              to {COMPLETION_TIMEOUT_MAX_S}; default 60.
            </p>
            {timeoutError ? (
              <p
                id="project-trace-timeout-error"
                role="alert"
                className="text-caption text-destructive"
              >
                {timeoutError}
              </p>
            ) : null}
          </div>
          <FormError message={timeoutFormError} />
          <FormSuccess message={timeoutSaved ? "Saved" : undefined} />
          <DisabledReason reason={canEdit ? undefined : reason}>
            <SubmitButton pending={timeoutMutation.isPending} disabled={!canEdit}>
              Save timeout
            </SubmitButton>
          </DisabledReason>
        </form>
      </section>
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
