// SPDX-License-Identifier: Elastic-2.0
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { orgScimQuery } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { CopyButton } from "@/components/copy-button";
import { DisabledReason } from "@/components/disabled-reason";
import { FormError } from "@/components/form/form-error";
import { LockedFeature } from "@/components/plan/locked-feature";
import { useOrgEntitlements } from "@/components/plan/use-org-entitlements";
import { SettingsCard } from "@/components/settings/settings-card";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import { TokensSection } from "@/ee/scim/tokens-section";
import { roleLabel } from "@/lib/roles";

const DEFAULT_ROLES: Schema<"OrgRole">[] = ["viewer", "member"];
const NEEDS_DOMAIN = "Verify a domain on the Single sign-on page first";

/**
 * The org's SCIM provisioning (owners only; the route checks the role). Without `scim` the
 * page only says what the feature offers (D-P-61).
 */
export function ProvisioningSettings({ org }: { org: string }) {
  const entitlements = useOrgEntitlements(org);
  const query = useQuery(orgScimQuery(org));

  if (entitlements.locked("scim")) {
    return <LockedFeature feature="scim" upgradeUrl={entitlements.upgradeUrl} />;
  }
  if (query.isPending) {
    return <Skeleton className="h-64 w-full" />;
  }
  if (query.data === undefined) {
    return <FormError message={problemMessage(query.error, "Could not load provisioning.")} />;
  }

  const scim = query.data;
  return (
    <div className="flex flex-col gap-6">
      <StatusCard org={org} scim={scim} />
      <TokensSection org={org} tokens={scim.tokens} />
    </div>
  );
}

/** The on/off switch, the default role and the base URL the identity provider needs. */
function StatusCard({ org, scim }: { org: string; scim: Schema<"ScimSettings"> }) {
  const queryClient = useQueryClient();
  const update = $api.useMutation("patch", "/api/v1/orgs/{org}/scim", {
    onSuccess: (answer) => {
      queryClient.setQueryData(orgScimQuery(org).queryKey, answer);
    },
  });

  // Turning SCIM off always works; turning it on needs a verified domain (D-EA-18).
  const switchOffWithoutDomain = !scim.enabled && scim.verified_domains === 0;
  const error = update.error
    ? problemMessage(update.error, "Could not change provisioning.")
    : undefined;

  function change(body: Schema<"UpdateScimSettings">) {
    update.reset();
    update.mutate({ params: { path: { org } }, body });
  }

  return (
    <SettingsCard title="Status">
      <div className="flex flex-col gap-3">
        <div className="flex items-center gap-2">
          <DisabledReason reason={switchOffWithoutDomain ? NEEDS_DOMAIN : undefined}>
            <Switch
              id="scim-enabled"
              checked={scim.enabled}
              disabled={update.isPending || switchOffWithoutDomain}
              onCheckedChange={(enabled) => {
                change({ enabled });
              }}
            />
          </DisabledReason>
          <Label htmlFor="scim-enabled">Enabled</Label>
        </div>
        <p className="text-caption text-muted-foreground">
          Your identity provider creates, updates and removes members over SCIM while this is on.
        </p>
      </div>
      <div className="flex flex-col gap-2">
        <Label htmlFor="scim-default-role">Default role</Label>
        <p className="text-caption text-muted-foreground">
          New members get this role. People who are already members keep theirs.
        </p>
        <Select
          value={scim.default_role}
          onValueChange={(value) => {
            const next = DEFAULT_ROLES.find((role) => role === value);
            if (next) {
              change({ default_role: next });
            }
          }}
          disabled={update.isPending}
        >
          <SelectTrigger id="scim-default-role" className="w-fit">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {DEFAULT_ROLES.map((role) => (
              <SelectItem key={role} value={role}>
                {roleLabel(role)}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
      <div className="flex flex-col gap-1">
        <p className="text-caption text-muted-foreground">Base URL</p>
        <div className="flex items-center gap-2">
          <code className="break-all font-mono text-caption">{scim.base_url}</code>
          <CopyButton value={scim.base_url} label="base URL" />
        </div>
      </div>
      <FormError message={error} />
    </SettingsCard>
  );
}
