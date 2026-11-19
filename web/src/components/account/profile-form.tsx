import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage } from "@/api/problem";
import { meQuery } from "@/api/queries";
import { TextField } from "@/components/form/text-field";
import { SettingsFormCard } from "@/components/settings/settings-card";

export function ProfileForm() {
  const { data: me } = $api.useSuspenseQuery("get", "/api/auth/me");
  const queryClient = useQueryClient();
  const save = $api.useMutation("patch", "/api/v1/me");
  const [name, setName] = useState(me.user.name ?? "");
  const [saved, setSaved] = useState(false);

  const trimmed = name.trim();
  const dirty = trimmed !== (me.user.name ?? "");
  const nameError = fieldErrors(save.error).name;

  function onSubmit() {
    setSaved(false);
    save.mutate(
      { body: trimmed.length > 0 ? { name: trimmed } : {} },
      {
        onSuccess: () => {
          setName(trimmed);
          setSaved(true);
          void queryClient.invalidateQueries({ queryKey: meQuery.queryKey });
        },
      },
    );
  }

  return (
    <SettingsFormCard
      title="Profile"
      description="How your name shows across cetra."
      onSubmit={onSubmit}
      dirty={dirty}
      pending={save.isPending}
      saved={saved}
      error={
        save.error && !nameError
          ? problemMessage(save.error, "Could not save your name. Try again.")
          : undefined
      }
    >
      <TextField
        id="profile-email"
        label="Email"
        type="email"
        value={me.user.email}
        readOnly
        onChange={() => {}}
        description="Your email can't be changed here."
      />
      <TextField
        id="profile-name"
        label="Display name (optional)"
        value={name}
        maxLength={100}
        autoComplete="name"
        description="Leave empty to show your email instead."
        error={nameError}
        disabled={save.isPending}
        onChange={(value) => {
          setName(value);
          setSaved(false);
        }}
      />
    </SettingsFormCard>
  );
}
