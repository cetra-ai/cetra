// SPDX-License-Identifier: Elastic-2.0
import type { Schema } from "@/api/types";
import { ReadOnlyCodeField } from "@/components/settings/read-only-code-field";
import { SettingsCard } from "@/components/settings/settings-card";

/** The values an owner enters in the identity provider (D-SA-12). */
export function SamlSpCard({ sp }: { sp: Schema<"OrgSsoSamlSp"> }) {
  return (
    <SettingsCard
      title="Service provider details"
      description="Enter these values in your identity provider's SAML app."
    >
      <ReadOnlyCodeField id="saml-entity-id" label="Entity ID (audience)" value={sp.entity_id} />
      <ReadOnlyCodeField id="saml-acs-url" label="ACS URL" value={sp.acs_url} />
      <ReadOnlyCodeField
        id="saml-metadata-url"
        label="SP metadata URL"
        value={sp.entity_id}
        description="Some identity providers can import these settings from this URL."
      />
      <ReadOnlyCodeField
        id="saml-start-url"
        label="Start URL"
        value={sp.start_url}
        description="Use it as the app's sign-on URL or tile link."
      />
    </SettingsCard>
  );
}
