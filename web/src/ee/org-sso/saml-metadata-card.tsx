// SPDX-License-Identifier: Elastic-2.0
import type { Schema } from "@/api/types";
import { RelativeTime } from "@/components/relative-time";
import { ReadOnlyCodeField } from "@/components/settings/read-only-code-field";
import { SettingsCard } from "@/components/settings/settings-card";
import { StatusPill } from "@/components/settings/status-pill";
import { TimeText } from "@/components/time-text";
import { certificateState, refreshErrorMessage } from "@/ee/org-sso/saml";

/** The identity provider's settings as read from its metadata, and the trusted certificates (D-SA-11). */
export function SamlMetadataCard({ saml }: { saml: Schema<"OrgSsoSaml"> }) {
  return (
    <SettingsCard title="Identity provider metadata">
      <ReadOnlyCodeField id="saml-idp-entity-id" label="IdP entity ID" value={saml.idp_entity_id} />
      <ReadOnlyCodeField id="saml-sso-url" label="Sign-on URL" value={saml.sso_url} />
      {saml.metadata_url ? (
        <div className="flex flex-col gap-2">
          <ReadOnlyCodeField
            id="saml-idp-metadata-url"
            label="Metadata URL"
            value={saml.metadata_url}
          />
          <div className="flex flex-wrap items-center gap-2 text-caption text-muted-foreground">
            {saml.refresh_error ? (
              <>
                <StatusPill tone="warn">Refresh failed</StatusPill>
                <span>{refreshErrorMessage(saml.refresh_error)}</span>
              </>
            ) : saml.refreshed_at ? (
              <span>
                Refreshed <RelativeTime value={saml.refreshed_at} />. It is checked once a day.
              </span>
            ) : null}
          </div>
        </div>
      ) : null}
      {saml.certificates_changed_at ? (
        <p role="note" className="text-body text-muted-foreground">
          Certificates changed on{" "}
          <TimeText value={saml.certificates_changed_at} style="datetime" />.
        </p>
      ) : null}
      <ul aria-label="Trusted certificates" className="flex flex-col gap-2">
        {saml.certificates.map((certificate) => {
          const state = certificateState(certificate, Date.now());
          return (
            <li
              key={certificate.sha256}
              className="flex flex-wrap items-center gap-2 text-body"
            >
              <span>
                SHA-256{" "}
                <code title={certificate.sha256} className="font-mono">
                  {certificate.sha256.slice(0, 16)}…
                </code>
              </span>
              {state.kind === "unreadable" ? (
                <StatusPill tone="error">Unreadable</StatusPill>
              ) : state.kind === "expired" ? (
                <StatusPill tone="error">Expired</StatusPill>
              ) : state.kind === "expiring" ? (
                <StatusPill tone="warn">
                  {`Expires in ${state.days} day${state.days === 1 ? "" : "s"}`}
                </StatusPill>
              ) : (
                <>
                  <StatusPill tone="ok">Valid</StatusPill>
                  {" until "}
                  <TimeText value={certificate.not_after} style="datetime" />
                </>
              )}
            </li>
          );
        })}
      </ul>
      <p className="text-caption text-muted-foreground">
        An expired certificate still verifies sign-ins. Add the new certificate before your identity
        provider switches to it.
      </p>
    </SettingsCard>
  );
}
