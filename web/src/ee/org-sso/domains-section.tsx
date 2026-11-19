// SPDX-License-Identifier: Elastic-2.0
import { useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";
import { $api } from "@/api/client";
import { fieldErrors, problemMessage } from "@/api/problem";
import type { Schema } from "@/api/types";
import { CopyButton } from "@/components/copy-button";
import { DataTable, type DataTableColumn } from "@/components/data-table/data-table";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { TextField } from "@/components/form/text-field";
import { SettingsCard } from "@/components/settings/settings-card";
import { SettingsTableFrame } from "@/components/settings/settings-list";
import { StatusPill } from "@/components/settings/status-pill";
import { TypeToConfirmDialog } from "@/components/type-to-confirm-dialog";
import { Button } from "@/components/ui/button";
import { storeOrgSso } from "@/ee/org-sso/cache";

const LOCKED_REASON = "Needs the Teams add-on or an enterprise licence";

type Domain = Schema<"OrgSsoDomain">;

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
  const [domainTouched, setDomainTouched] = useState(false);
  const [pendingRemoval, setPendingRemoval] = useState<Domain | undefined>();
  const add = $api.useMutation("post", "/api/v1/orgs/{org}/sso/domains");
  const verify = $api.useMutation("post", "/api/v1/orgs/{org}/sso/domains/{id}/verify");
  const remove = $api.useMutation("delete", "/api/v1/orgs/{org}/sso/domains/{id}");

  const addFieldErrors = fieldErrors(add.error);
  const domainRequired = domain.trim() === "" ? "Enter a domain." : undefined;
  const domainError = addFieldErrors.domain ?? (domainTouched ? domainRequired : undefined);
  const addError =
    add.error && addFieldErrors.domain === undefined
      ? problemMessage(add.error, "Could not add the domain.")
      : undefined;

  function onAdd(event: FormEvent) {
    event.preventDefault();
    setDomainTouched(true);
    add.reset();
    if (domainRequired !== undefined) {
      return;
    }
    add.mutate(
      { params: { path: { org } }, body: { domain: domain.trim() } },
      {
        onSuccess: (answer) => {
          setDomain("");
          setDomainTouched(false);
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

  const columns: DataTableColumn<Domain>[] = [
    {
      id: "domain",
      size: 200,
      header: "Domain",
      enableSorting: false,
      cell: ({ row }) => <span className="font-medium">{row.original.domain}</span>,
    },
    {
      id: "status",
      size: 120,
      header: "Status",
      enableSorting: false,
      cell: ({ row }) =>
        row.original.status === "verified" ? (
          <StatusPill tone="ok">Verified</StatusPill>
        ) : (
          <StatusPill tone="warn">Pending</StatusPill>
        ),
    },
    {
      id: "txt",
      size: 380,
      header: "DNS TXT record",
      enableSorting: false,
      meta: { grow: true },
      cell: ({ row }) =>
        row.original.status === "pending" ? (
          <div className="flex flex-col gap-1 text-caption">
            <span className="flex items-center gap-1">
              <code className="break-all">{row.original.txt_name}</code>
              <CopyButton value={row.original.txt_name} label="TXT name" />
            </span>
            <span className="flex items-center gap-1">
              <code className="break-all">{row.original.txt_value}</code>
              <CopyButton value={row.original.txt_value} label="TXT value" />
            </span>
          </div>
        ) : null,
    },
    {
      id: "actions",
      size: 168,
      header: "Actions",
      enableSorting: false,
      meta: { align: "right" },
      cell: ({ row }) => (
        <div className="flex justify-end gap-2">
          {row.original.status === "pending" ? (
            <Button
              type="button"
              variant="outline"
              size="sm"
              disabled={locked || verify.isPending}
              onClick={() => {
                onVerify(row.original.id);
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
              setPendingRemoval(row.original);
            }}
          >
            Remove
          </Button>
        </div>
      ),
    },
  ];

  return (
    <SettingsCard
      title="Domains"
      description="People with these email domains sign in through the provider. Prove each domain with its DNS TXT record, then verify it."
    >
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
          error={domainError}
          onChange={(value) => {
            setDomain(value);
            setDomainTouched(true);
          }}
        />
        <FormError message={addError} />
        <DisabledReason reason={locked ? LOCKED_REASON : undefined}>
          <Button
            type="submit"
            variant="outline"
            disabled={locked || add.isPending || domainRequired !== undefined}
          >
            Add domain
          </Button>
        </DisabledReason>
      </form>
      <SettingsTableFrame rowCount={sso.domains.length}>
        {sso.domains.length === 0 ? (
          <p className="p-3 text-body text-muted-foreground">No domains yet.</p>
        ) : (
          <DataTable
            tableId="sso-domains"
            ariaLabel="Domains"
            columns={columns}
            rows={sso.domains}
            getRowId={(row) => row.id}
            onActivate={() => undefined}
            hasNextPage={false}
            isFetchingNextPage={false}
            fetchNextPage={() => undefined}
            emptyState={null}
          />
        )}
      </SettingsTableFrame>
      <FormError message={verifyError} />
      <TypeToConfirmDialog
        open={pendingRemoval !== undefined}
        onOpenChange={(open) => {
          if (!open) {
            setPendingRemoval(undefined);
          }
        }}
        title={`Remove ${pendingRemoval?.domain ?? "domain"}?`}
        description="People with this domain's emails will no longer be sent to your identity provider."
        expected={pendingRemoval?.domain ?? ""}
        inputLabel={`Type "${pendingRemoval?.domain ?? ""}" to confirm`}
        confirmLabel="Remove domain"
        pending={remove.isPending}
        error={removeError}
        onConfirm={confirmRemove}
      />
    </SettingsCard>
  );
}
