// SPDX-License-Identifier: Elastic-2.0
import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { orgScimQuery } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import { RelativeTime } from "@/components/relative-time";
import { SecretDialog } from "@/components/secret-dialog";
import { SettingsCard } from "@/components/settings/settings-card";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
import { Button } from "@/components/ui/button";

/** The server keeps at most two live tokens (D-EA-15). */
const MAX_TOKENS = 2;

type Token = Schema<"ScimToken">;

/**
 * The org's SCIM bearer tokens: create (the secret is shown once) and revoke. `revokeOnly` (a
 * lapsed `scim` feature, D-EA-19) hides the create form, so only revoking is offered.
 */
export function TokensSection({
  org,
  tokens,
  revokeOnly = false,
}: {
  org: string;
  tokens: Token[];
  revokeOnly?: boolean;
}) {
  const queryClient = useQueryClient();
  const [name, setName] = useState("");
  const [secret, setSecret] = useState<string | undefined>();
  const [revoking, setRevoking] = useState<Token | undefined>();

  const create = $api.useMutation("post", "/api/v1/orgs/{org}/scim/tokens", {
    onSuccess: (answer) => {
      setName("");
      setSecret(answer.secret);
      void queryClient.invalidateQueries({ queryKey: orgScimQuery(org).queryKey });
    },
  });
  const revoke = $api.useMutation("delete", "/api/v1/orgs/{org}/scim/tokens/{id}", {
    onSuccess: () => {
      setRevoking(undefined);
      void queryClient.invalidateQueries({ queryKey: orgScimQuery(org).queryKey });
    },
  });

  const full = tokens.length >= MAX_TOKENS;
  const createReason = full ? "Two tokens are live. Revoke one to create another." : undefined;
  const createError = create.error
    ? problemMessage(create.error, "Could not create the token.")
    : undefined;

  return (
    <SettingsCard
      title="Tokens"
      description="Your identity provider signs in with a token. Two can be live at once."
    >
      {tokens.length === 0 ? (
        <p className="text-body text-muted-foreground">No tokens yet.</p>
      ) : (
        <table aria-label="SCIM tokens" className="w-full text-left text-body">
          <thead className="text-caption text-muted-foreground">
            <tr>
              <th scope="col" className="py-1 pr-3 font-medium">
                Name
              </th>
              <th scope="col" className="py-1 pr-3 font-medium">
                Prefix
              </th>
              <th scope="col" className="py-1 pr-3 font-medium">
                Created
              </th>
              <th scope="col" className="py-1 pr-3 font-medium">
                Last used
              </th>
              <th scope="col" className="py-1">
                <span className="sr-only">Actions</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {tokens.map((token) => (
              <tr key={token.id} className="border-t">
                <td className="py-2 pr-3">{token.name}</td>
                <td className="py-2 pr-3 font-mono text-caption">{token.prefix}</td>
                <td className="py-2 pr-3">
                  <RelativeTime value={token.created_at} />
                </td>
                <td className="py-2 pr-3">
                  {token.last_used_at ? <RelativeTime value={token.last_used_at} /> : "Never"}
                </td>
                <td className="py-2 text-right">
                  <Button
                    type="button"
                    variant="outline"
                    onClick={() => {
                      revoke.reset();
                      setRevoking(token);
                    }}
                  >
                    Revoke
                  </Button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {revokeOnly ? null : (
        <form
          noValidate
          aria-label="Create a token"
          className="flex flex-col gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            if (full || name.trim() === "") {
              return;
            }
            create.reset();
            create.mutate({ params: { path: { org } }, body: { name: name.trim() } });
          }}
        >
          <div className="flex flex-wrap items-end gap-2">
            <TextField
              id="scim-token-name"
              label="Token name"
              value={name}
              maxLength={100}
              disabled={full || create.isPending}
              onChange={setName}
            />
            <DisabledReason reason={createReason}>
              <SubmitButton pending={create.isPending} disabled={full || name.trim() === ""}>
                Create token
              </SubmitButton>
            </DisabledReason>
          </div>
          <FormError message={createError} />
        </form>
      )}
      <SecretDialog
        open={secret !== undefined}
        title="Token created"
        secret={secret ?? ""}
        description="Paste it into your identity provider."
        onDone={() => {
          setSecret(undefined);
        }}
      />
      <TypeToConfirmDialog
        open={revoking !== undefined}
        onOpenChange={(next) => {
          if (!next) {
            setRevoking(undefined);
            revoke.reset();
          }
        }}
        title="Revoke token?"
        description={`${revoking?.name ?? "This token"} stops working at once. Its identity provider can no longer provision members with it.`}
        expected="revoke"
        inputLabel='Type "revoke" to confirm'
        confirmLabel="Revoke"
        pending={revoke.isPending}
        error={
          revoke.error ? problemMessage(revoke.error, "Could not revoke the token.") : undefined
        }
        onConfirm={() => {
          if (revoking !== undefined) {
            revoke.mutate({ params: { path: { org, id: revoking.id } } });
          }
        }}
      />
    </SettingsCard>
  );
}
