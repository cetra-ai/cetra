// SPDX-License-Identifier: Elastic-2.0
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage, problemOf } from "@/api/problem";
import { projectQuery } from "@/api/queries";
import { projectMembersKey } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { FormError } from "@/components/form/form-error";
import { FormSuccess } from "@/components/form/form-success";
import { SubmitButton } from "@/components/form/submit-button";
import { LockedFeature } from "@/components/plan/locked-feature";
import { useOrgMembers } from "@/components/settings/org-members";
import { SettingsPanel } from "@/components/settings/settings-list";
import { UnsavedChangesGuard } from "@/components/settings/unsaved-changes";
import { Label } from "@/components/ui/label";
import { canHaveOverride, roleLabel } from "@/lib/roles";
import { type Access, ProjectAccessSelect } from "./project-access-select";

/** The row panel of one project member: the project access override, saved with its own Save. */
export function MemberAccessPanel({
  org,
  project,
  ref,
  member,
  selfId,
  canEdit,
  locked,
  upgradeUrl,
  onClose,
}: {
  org: string;
  project: string;
  /** The project reference the members API takes in its path. */
  ref: string;
  member: Schema<"ProjectMember">;
  selfId: string;
  canEdit: boolean;
  locked: boolean;
  upgradeUrl: string | undefined;
  onClose: () => void;
}) {
  const queryClient = useQueryClient();
  // The project member row has no SCIM flag; the org's member list does (D-EA-18).
  const managed =
    useOrgMembers(org)
      .data?.pages.flatMap((page) => page.data)
      .find((item) => item.user.id === member.user.id)?.scim_managed === true;
  const editable = canEdit && !managed;
  const stored: Access = member.override ?? "inherit";
  const [draft, setDraft] = useState<Access>(stored);
  const [actionError, setActionError] = useState<string | undefined>();
  const [saved, setSaved] = useState(false);

  function onChanged(user: string) {
    void queryClient.invalidateQueries({ queryKey: projectMembersKey(org, project) });
    if (user === selfId) {
      void queryClient.invalidateQueries({ queryKey: projectQuery(org, project).queryKey });
    }
  }

  function onSaved(user: string) {
    setSaved(true);
    onChanged(user);
  }

  function onFailed(error: unknown) {
    if (problemOf(error)?.code === "plan_required") {
      setActionError(problemMessage(error, ""));
      return;
    }
    setActionError(
      problemOf(error)?.status === 403
        ? "Only project admins can change access."
        : problemMessage(error, "Could not change access."),
    );
  }

  const put = $api.useMutation("put", "/api/v1/projects/{project}/members/{user}", {
    onSuccess: (_data, variables) => onSaved(variables.params.path.user),
    onError: onFailed,
  });
  const del = $api.useMutation("delete", "/api/v1/projects/{project}/members/{user}", {
    onSuccess: (_data, variables) => onSaved(variables.params.path.user),
    onError: onFailed,
  });

  const pending = put.isPending || del.isPending;
  const dirty = draft !== stored;
  const overridable = canHaveOverride(member.org_role);

  function save() {
    setActionError(undefined);
    setSaved(false);
    const path = { project: ref, user: member.user.id };
    if (draft === "inherit") {
      del.mutate({ params: { path } });
      return;
    }
    put.mutate({ params: { path }, body: { role: draft } });
  }

  return (
    <SettingsPanel
      title={member.user.name ?? member.user.email}
      subtitle={member.user.name ? member.user.email : undefined}
      onClose={onClose}
      footer={
        overridable && editable ? (
          <>
            {saved && !dirty ? <FormSuccess message="Saved" /> : null}
            {dirty ? (
              <span className="mr-auto text-caption text-muted-foreground">Unsaved changes</span>
            ) : null}
            <SubmitButton form="member-access-form" pending={pending} disabled={!dirty}>
              Save
            </SubmitButton>
          </>
        ) : undefined
      }
    >
      <dl className="flex flex-col gap-3 text-body">
        <div className="flex flex-col gap-0.5">
          <dt className="text-caption text-muted-foreground">Org role</dt>
          <dd>{roleLabel(member.org_role)}</dd>
        </div>
        <div className="flex flex-col gap-0.5">
          <dt className="text-caption text-muted-foreground">Effective role</dt>
          <dd>{member.effective_role ? roleLabel(member.effective_role) : "No access"}</dd>
        </div>
      </dl>
      {overridable ? (
        <form
          id="member-access-form"
          noValidate
          className="flex flex-col gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            save();
          }}
        >
          <Label htmlFor="member-access">Project access</Label>
          <p className="text-caption text-muted-foreground">
            Overrides the org role in this project.
          </p>
          {managed ? (
            <p className="text-caption text-muted-foreground">
              Managed by your identity provider. Project access can't be changed by hand.
            </p>
          ) : null}
          <ProjectAccessSelect
            id="member-access"
            email={member.user.email}
            stored={stored}
            value={draft}
            canEdit={editable}
            locked={locked}
            pending={pending}
            onChange={(value) => {
              setSaved(false);
              setActionError(undefined);
              setDraft(value);
            }}
          />
          <FormError message={actionError} />
        </form>
      ) : (
        <p className="text-body text-muted-foreground">
          Owners and admins always have admin access to every project.
        </p>
      )}
      {locked && canEdit ? <LockedFeature feature="project_roles" upgradeUrl={upgradeUrl} /> : null}
      <UnsavedChangesGuard dirty={dirty && !pending} />
    </SettingsPanel>
  );
}
