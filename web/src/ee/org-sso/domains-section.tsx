// SPDX-License-Identifier: Elastic-2.0
import { useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage } from "@/api/problem";
import type { Schema } from "@/api/types";
import { ConfirmDialog } from "@/components/confirm-dialog";
import { CopyButton } from "@/components/copy-button";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { TextField } from "@/components/form/text-field";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { storeOrgSso } from "@/ee/org-sso/cache";

const LOCKED_REASON = "Needs the Teams add-on or an enterprise licence";

/**
 * The org's email domains and how each is proven with a DNS TXT record (D-P-54).
 * Removing a domain stays possible without `org_sso` (D-P-61).
 */
export function DomainsSection({
  org,
  sso,
  locked,
}: {
  org: string;
  sso: Schema<"OrgSso">;
  locked: boolean;
}) {
  const queryClient = useQueryClient();
  const [domain, setDomain] = useState("");
  const [pendingRemoval, setPendingRemoval] = useState<Schema<"OrgSsoDomain"> | undefined>();
  const add = $api.useMutation("post", "/api/v1/orgs/{org}/sso/domains");
  const verify = $api.useMutation("post", "/api/v1/orgs/{org}/sso/domains/{id}/verify");
  const remove = $api.useMutation("delete", "/api/v1/orgs/{org}/sso/domains/{id}");

  const addFieldErrors = fieldErrors(add.error);
  const addError =
    add.error && addFieldErrors.domain === undefined
      ? problemMessage(add.error, "Could not add the domain.")
      : undefined;

  function onAdd(event: FormEvent) {
    event.preventDefault();
    add.reset();
    add.mutate(
      { params: { path: { org } }, body: { domain: domain.trim() } },
      {
        onSuccess: (answer) => {
          setDomain("");
          storeOrgSso(queryClient, org, answer);
        },
      },
    );
  }

  function onVerify(id: string) {
    verify.reset();
    verify.mutate(
      { params: { path: { org, id } } },
      {
        onSuccess: (answer) => {
          storeOrgSso(queryClient, org, answer);
        },
      },
    );
  }

  function confirmRemove() {
    if (pendingRemoval === undefined) {
      return;
    }
    remove.mutate(
      { params: { path: { org, id: pendingRemoval.id } } },
      {
        onSuccess: () => {
          setPendingRemoval(undefined);
          storeOrgSso(queryClient, org, undefined);
        },
      },
    );
  }

  const verifyError = verify.error
    ? problemMessage(verify.error, "Could not verify the domain.")
    : undefined;
  const removeError = remove.error
    ? problemMessage(remove.error, "Could not remove the domain.")
    : undefined;

  return (
    <section aria-labelledby="sso-domains-heading" className="flex flex-col gap-4">
      <div className="flex flex-col gap-1">
        <h2 id="sso-domains-heading" className="text-base font-medium">
          Domains
        </h2>
        <p className="text-sm text-muted-foreground">
          People with these email domains sign in through the provider. Prove each domain with its
          DNS TXT record, then verify it.
        </p>
      </div>
      <form
        noValidate
        aria-label="Add domain"
        className="flex max-w-md flex-col gap-4"
        onSubmit={onAdd}
      >
        <TextField
          id="sso-domain"
          label="Domain"
          value={domain}
          placeholder="example.com"
          disabled={locked || add.isPending}
          error={addFieldErrors.domain}
          onChange={setDomain}
        />
        <FormError message={addError} />
        <DisabledReason reason={locked ? LOCKED_REASON : undefined}>
          <Button type="submit" variant="outline" disabled={locked || add.isPending}>
            Add domain
          </Button>
        </DisabledReason>
      </form>
      <Table aria-label="Domains">
        <TableHeader>
          <TableRow>
            <TableHead>Domain</TableHead>
            <TableHead>Status</TableHead>
            <TableHead>DNS TXT record</TableHead>
            <TableHead>
              <span className="sr-only">Actions</span>
            </TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {sso.domains.length === 0 ? (
            <TableRow>
              <TableCell colSpan={4} className="text-muted-foreground">
                No domains yet.
              </TableCell>
            </TableRow>
          ) : null}
          {sso.domains.map((row) => (
            <TableRow key={row.id}>
              <TableCell className="font-medium">{row.domain}</TableCell>
              <TableCell>
                {row.status === "verified" ? (
                  <Badge>Verified</Badge>
                ) : (
                  <Badge variant="outline">Pending</Badge>
                )}
              </TableCell>
              <TableCell>
                {row.status === "pending" ? (
                  <div className="flex flex-col gap-1 text-xs">
                    <span className="flex items-center gap-1">
                      <code className="break-all">{row.txt_name}</code>
                      <CopyButton value={row.txt_name} label="TXT name" />
                    </span>
                    <span className="flex items-center gap-1">
                      <code className="break-all">{row.txt_value}</code>
                      <CopyButton value={row.txt_value} label="TXT value" />
                    </span>
                  </div>
                ) : null}
              </TableCell>
              <TableCell>
                <div className="flex justify-end gap-2">
                  {row.status === "pending" ? (
                    <Button
                      type="button"
                      variant="outline"
                      size="sm"
                      disabled={locked || verify.isPending}
                      onClick={() => {
                        onVerify(row.id);
                      }}
                    >
                      Verify
                    </Button>
                  ) : null}
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    onClick={() => {
                      remove.reset();
                      setPendingRemoval(row);
                    }}
                  >
                    Remove
                  </Button>
                </div>
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
      <FormError message={verifyError} />
      <ConfirmDialog
        open={pendingRemoval !== undefined}
        onOpenChange={(open) => {
          if (!open) {
            setPendingRemoval(undefined);
          }
        }}
        title={`Remove ${pendingRemoval?.domain ?? "domain"}?`}
        description="People with this domain's emails will no longer be sent to your identity provider."
        confirmLabel="Remove domain"
        destructive
        pending={remove.isPending}
        error={removeError}
        onConfirm={confirmRemove}
      />
    </section>
  );
}
