import { useQueryClient } from "@tanstack/react-query";
import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage, problemOf } from "@/api/problem";
import { meQuery } from "@/api/queries";
import { orgInit, orgQuery } from "@/api/queries-settings";
import { DangerZone } from "@/components/deletions/danger-zone";
import { TextField } from "@/components/form/text-field";
import { ReadOnlyCodeField } from "@/components/settings/read-only-code-field";
import { SettingsFormCard } from "@/components/settings/settings-card";
import { SettingsPage } from "@/components/settings/settings-page";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
import { atLeast } from "@/lib/roles";

export const Route = createFileRoute("/$org/settings/general")({
  loader: ({ context, params }) => context.queryClient.ensureQueryData(orgQuery(params.org)),
  component: OrgGeneralPage,
});

function OrgGeneralPage() {
  const { org } = Route.useParams();
  const { data } = $api.useSuspenseQuery("get", "/api/v1/orgs/{org}", orgInit(org));
  const canEdit = atLeast(data.role, "admin");
  const [name, setName] = useState(data.name);
  const [touched, setTouched] = useState(false);
  const [serverError, setServerError] = useState<string | undefined>();
  const [error, setError] = useState<string | undefined>();
  const [saved, setSaved] = useState(false);
  const mutation = $api.useMutation("patch", "/api/v1/orgs/{org}");
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const [dropOpen, setDropOpen] = useState(false);
  const dropMutation = $api.useMutation("delete", "/api/v1/orgs/{org}");
  const canDelete = data.role === "owner";

  const trimmed = name.trim();
  const clientError = trimmed.length === 0 ? "Enter a name." : undefined;
  const dirty = trimmed !== data.name;

  function confirmDrop() {
    dropMutation.mutate(
      { params: { path: { org }, query: { confirm: data.slug } } },
      {
        onSuccess: async () => {
          setDropOpen(false);
          // `/` redirects to the first org in `me`, so `me` must be refetched before leaving.
          await queryClient.invalidateQueries({ queryKey: meQuery.queryKey });
          await navigate({ to: "/", ignoreBlocker: true });
          queryClient.removeQueries({ queryKey: orgQuery(org).queryKey });
        },
      },
    );
  }

  const dropError = dropMutation.error
    ? problemOf(dropMutation.error)?.status === 403
      ? "Only owners can delete the organization."
      : problemMessage(dropMutation.error, "Could not delete the organization.")
    : undefined;

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
      { params: { path: { org } }, body: { name: trimmed } },
      {
        onSuccess: () => {
          setSaved(true);
          void queryClient.invalidateQueries({ queryKey: orgQuery(org).queryKey });
          // The top bar shows the name from `me`.
          void queryClient.invalidateQueries({ queryKey: meQuery.queryKey });
        },
        onError: (caught) => {
          const message = fieldErrors(caught).name;
          if (message !== undefined) {
            setServerError(message);
            return;
          }
          if (problemOf(caught)?.status === 403) {
            setError("Only admins and owners can rename the organization.");
            return;
          }
          setError(problemMessage(caught, "Could not save the organization."));
        },
      },
    );
  }

  return (
    <SettingsPage
      title="General"
      description="Name and URL of the organization. Project names and retention are in each project's General."
    >
      <SettingsFormCard
        title="Organization"
        dirty={dirty}
        invalid={clientError !== undefined}
        pending={mutation.isPending}
        saved={saved}
        error={error}
        disabledReason={canEdit ? undefined : "Only admins and owners can rename the organization"}
        onSubmit={onSubmit}
      >
        <TextField
          id="org-name"
          label="Name"
          value={name}
          required
          maxLength={100}
          disabled={!canEdit || mutation.isPending}
          error={(touched ? clientError : undefined) ?? serverError}
          onChange={(value) => {
            setName(value);
            setTouched(true);
            setSaved(false);
            setServerError(undefined);
          }}
        />
        <ReadOnlyCodeField
          id="org-slug"
          label="Slug"
          value={data.slug}
          description="Part of every URL in this organization; cannot be changed."
        />
      </SettingsFormCard>
      <DangerZone
        title="Delete organization"
        description="Deletes the organization, all its projects and their data, its members and invitations. Data is removed within 24 hours; stored copies are purged after the storage grace period. This cannot be undone."
        buttonLabel="Delete organization…"
        disabledReason={canDelete ? undefined : "Only owners can delete the organization"}
        onOpen={() => {
          dropMutation.reset();
          setDropOpen(true);
        }}
      >
        <TypeToConfirmDialog
          open={dropOpen}
          onOpenChange={setDropOpen}
          title={`Delete organization ${data.name}?`}
          description="Deletes the organization, all its projects and their data, its members and invitations. Data is removed within 24 hours; stored copies are purged after the storage grace period. This cannot be undone."
          expected={data.slug}
          inputLabel={`Type the organization slug "${data.slug}" to confirm`}
          confirmLabel="Delete organization"
          pending={dropMutation.isPending}
          error={dropError}
          onConfirm={confirmDrop}
        />
      </DangerZone>
    </SettingsPage>
  );
}
