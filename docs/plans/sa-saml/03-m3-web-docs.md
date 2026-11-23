# SA · M3 — Settings page and docs

Milestone 3 of track SA ([`README.md`](README.md)). It gives org owners the SAML settings in the web UI, adds the login messages for the two new codes, and writes the docs. It builds on M2 ([`02-m2-server.md`](02-m2-server.md)).

When M3 is done:
- the org's Single sign-on page offers a protocol choice (OpenID Connect or SAML 2.0) for its one provider, with the SAML form:
  - display name and JIT role;
  - the metadata from a URL, pasted XML, or manual fields;
  - optional attribute names;
- the page shows the values to copy into the IdP (entity ID, ACS URL, start URL, SP metadata URL), the trusted certificates with their expiry, and the metadata refresh status;
- changing the IdP entity ID asks for confirmation, and switching protocol warns first;
- without the `saml` feature the SAML form is locked, while turning things off keeps working (D-P-61);
- the login page explains `saml_invalid` and `saml_unsupported`;
- `platform/sso.mdx`, `plans-and-licence.mdx`, `self-hosting/authentication.mdx` and spec 11 describe SAML.

## Prerequisites

- M2 is implemented and its checks pass. `web/src/api/schema.gen.ts` contains `PutOrgSamlProvider`, `OrgSsoSaml`, `OrgSsoCertificate`, `SamlAttributeNames`, `SamlRefreshError`, `OrgSsoSamlSp`, `SsoProtocol`, and the path `/api/v1/orgs/{org}/sso/saml`. Check with `grep -n "sso/saml\|PutOrgSamlProvider" web/src/api/schema.gen.ts`; if missing, stop.
- The operation ID of the SAML PUT in `crates/server/openapi.json` is `put_org_saml_provider` (M2 SA2.7). M2 also registered it in `web/src/mocks/writes.ts`.
- **Publishing.** The track has one PR, merged only after M4 (`roadmap.md`; the user's one-PR-per-track rule). So nothing in this milestone is published before the review and interop of M4. M3 writes the docs as final. M4 corrects them after interop. That settles the README's "draft" wording: the draft is what this branch holds until M4.

## Read first

1. `AGENTS.md` (web rules: MSW mocks, `server.use` in page tests; docs rules), `website/STYLE.md` (all of it).
2. [`README.md`](README.md): D-SA-2, D-SA-5, D-SA-6, D-SA-11, D-SA-12, D-SA-18, D-SA-19.
3. [`02-m2-server.md`](02-m2-server.md) § SA2.4: the API, its field-error names and its 409 texts, which this UI relies on.
4. The web facts gathered for this plan: `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/sa-m3-web-facts.md` (read it whole).
5. The code:
   - `web/src/ee/org-sso/*`;
   - `web/src/routes/$org/settings/sso.tsx` and `sso.test.tsx`;
   - `web/src/components/settings/{settings-card,read-only-code-field,status-pill}.tsx`, `web/src/components/form/text-field.tsx`;
   - `web/src/components/ui/{tabs,radio-group,textarea}.tsx`, `web/src/components/confirm-dialog.tsx`, `web/src/components/time-text.tsx`;
   - `web/src/api/problem.ts`, `web/src/lib/search/auth.ts`, `web/src/components/auth/messages.ts`;
   - `website/src/content/docs/docs/platform/sso.mdx`.

## What it is, and what it is not

**It is:** web UI under `web/src/ee/org-sso/`, the login messages, the docs pages, spec 11, and the CHANGELOG line.

**It is not:** any server change (M2), the security review and interop (M4), or screenshots (the user, 2026-11-21: docs screenshots wait until the product is frozen).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-SA-2 | One provider; a protocol switch deletes identities and needs a new test sign-in; refused while enforced. |
| D-SA-6 | Attribute names are optional; blank uses the defaults. |
| D-SA-11 | Certificates are shown with their expiry and warned about 30 days ahead; refresh status; an entity ID change is an explicit, confirmed owner action. |
| D-SA-12 | The SP values: entity ID (also the SP metadata URL), ACS URL, start URL. |
| D-SA-18 | `PUT …/sso/saml`, exactly one source or none (none keeps the stored metadata, M2 SA2.4); `confirm_entity_id_change`. |
| D-SA-19 | https public URL; the API answers 409 otherwise, and the UI shows the message. |
| D-P-61 | Without the feature, turning things off and removing still work. |

## Versions

No new dependency.

## Phases and dependencies

| Phase | Edits | Depends on | Parallel with |
|---|---|---|---|
| SA3.1 Login messages | `web/src/lib/search/auth.ts`, `web/src/components/auth/messages.ts`, `web/src/routes/login.test.tsx` | M2 | SA3.2, SA3.3 |
| SA3.2 SAML settings UI | `web/src/ee/org-sso/{org-sso-settings,provider-section,saml-provider-form,saml-metadata-card,saml-sp-card}.tsx`, `web/src/ee/org-sso/saml.ts`, `web/src/ee/org-sso/saml.test.ts`, `web/src/routes/$org/settings/sso.test.tsx`, `web/src/routes/$org/settings/sso.saml.test.tsx` | M2 | SA3.1, SA3.3 |
| SA3.3 Docs and spec | `website/src/content/docs/docs/platform/sso.mdx`, `…/platform/plans-and-licence.mdx`, `…/self-hosting/authentication.mdx`, `docs/spec/11-ui.md`, `CHANGELOG.md` | M2 | SA3.1, SA3.2 |
| SA3.4 Checks and review (coordinating session) | fixes | all | — |

## Conventions for every phase

- **No checks while implementing (the user, 2026-11-20).** Do not run `pnpm`, vitest, `tsc`, `just check`, or the docs build. Write and commit. SA3.4 runs the checks once.
- Stage only your files, by path. If `git commit` fails on `index.lock`, wait and retry.
- Every new `.ts`/`.tsx` file under `web/src/ee/` starts with exactly `// SPDX-License-Identifier: Elastic-2.0`. Route and test files outside `ee` do not.
- Follow the existing components' patterns: controlled `useState`, `SettingsFormCard` for Save and the unsaved-changes guard, `fieldErrors` and `problemMessage` from `@/api/problem`, `storeOrgSso` after every write. No new UI library and no react-hook-form.
- Copy is in the docs' style (`website/STYLE.md`): second person, plain, no marketing words, "organization" in UI text, sentence case. Every user-facing string this plan gives is exact; use it verbatim.

---

## SA3.1 — Login messages

**Goal:** the login page explains `saml_invalid` and `saml_unsupported` (D-SA-12).

**Edit:** `web/src/lib/search/auth.ts`, `web/src/components/auth/messages.ts`, `web/src/routes/login.test.tsx`.

**Steps:**
1. `auth.ts`: append `"saml_invalid"`, `"saml_unsupported"` to the end of `OAUTH_ERROR_CODES`.
2. `messages.ts`: add to `OAUTH_MESSAGES`:
   - `saml_invalid: "The identity provider's sign-in response could not be accepted. Start again from the login page."`
   - `saml_unsupported: "The identity provider sent an encrypted assertion, which is not supported. Ask an owner of your organization to turn off assertion encryption in the identity provider."`
3. `login.test.tsx`: append `"saml_invalid"`, `"saml_unsupported"` to the `it.each([...])` list at the "explains the %s error from the URL" test (around line 482).

**Tests:** the extended `it.each` and the existing `messages.test.ts`, which loops every code.

**Done when:** written and committed.

**Commit:** `Explain the SAML login errors`

---

## SA3.2 — SAML settings UI

**Goal:** owners set up and maintain a SAML provider on the Single sign-on page.

**Edit:** `web/src/ee/org-sso/org-sso-settings.tsx`, `web/src/ee/org-sso/provider-section.tsx`, `web/src/ee/org-sso/saml-provider-form.tsx` (new), `web/src/ee/org-sso/saml-metadata-card.tsx` (new), `web/src/ee/org-sso/saml-sp-card.tsx` (new), `web/src/ee/org-sso/saml.ts` (new), `web/src/ee/org-sso/saml.test.ts` (new), `web/src/routes/$org/settings/sso.test.tsx`, `web/src/routes/$org/settings/sso.saml.test.tsx` (new).

**Generated types.** Follow `web/src/api/schema.gen.ts` exactly: where a field is `T | null` rather than optional, typed literals use `null`. Read the generated `OrgSsoSaml`, `OrgSsoProvider` and `PutOrgSamlProvider` before writing.

**Steps:**

1. `saml.ts` (pure helpers, `ee` header):
   ```ts
   import type { Schema } from "@/api/types";

   /** The 409 detail the API sends when an entity ID change needs confirmation (M2 SA2.4). */
   export const ENTITY_ID_CHANGE_DETAIL =
     "Changing the IdP entity ID deletes the linked identities; confirm the change.";

   /** Days before `not_after` from which a certificate is shown as expiring (D-SA-11). */
   export const EXPIRY_WARNING_DAYS = 30;

   export type CertificateState =
     | { kind: "unreadable" }
     | { kind: "expired" }
     | { kind: "expiring"; days: number }
     | { kind: "valid" };

   /** The display state of a certificate at `nowMs` (epoch dates mean it no longer reads). */
   export function certificateState(certificate: Schema<"OrgSsoCertificate">, nowMs: number): CertificateState;

   /** Splits pasted PEM text into certificates: one per BEGIN block, or the whole text when it has none. Blank input is []. */
   export function splitCertificates(text: string): string[];

   /** The message for a stored refresh failure (D-SA-11). */
   export function refreshErrorMessage(error: Schema<"SamlRefreshError">): string;

   /** The attributes object of a PUT body: only the non-blank, trimmed names. */
   export function attributesBody(names: Schema<"SamlAttributeNames">): Schema<"SamlAttributeNames">;
   ```
   - `certificateState`:
     - `Date.parse(not_after) <= 0` and `Date.parse(not_before) <= 0` → `unreadable`;
     - `Date.parse(not_after) <= nowMs` → `expired`;
     - `days = Math.ceil((Date.parse(not_after) - nowMs) / 86_400_000)`; `days <= EXPIRY_WARNING_DAYS` → `expiring`;
     - otherwise `valid`.
   - `splitCertificates`:
     - Trim the text. If it contains `-----BEGIN CERTIFICATE-----`, split it with `/(?=-----BEGIN CERTIFICATE-----)/`, trim each piece, and keep only the pieces that start with `-----BEGIN CERTIFICATE-----` (text before the first block is dropped).
     - Else, a non-empty text is one entry.
   - `refreshErrorMessage`:
     - `fetch_failed` → "The metadata URL could not be reached."
     - `status` → "The metadata URL answered with an error."
     - `too_large` → "The metadata is larger than 1 MiB."
     - `denied` → "The metadata URL is not allowed by this instance's network policy."
     - `unusable` → "The metadata could not be used."
     - `entity_id_changed` → "The metadata now names another entity ID. Save it again to accept the change."
   - `attributesBody`: keep each of the four keys only when its value, trimmed, is non-empty; send the trimmed value.

2. `saml-sp-card.tsx`: `export function SamlSpCard({ sp }: { sp: Schema<"OrgSsoSamlSp"> })` renders a `SettingsCard` with title `"Service provider details"` and description `"Enter these values in your identity provider's SAML app."`. It holds four `ReadOnlyCodeField`s:
   - `id="saml-entity-id"`, label `"Entity ID (audience)"`, `sp.entity_id`;
   - `id="saml-acs-url"`, label `"ACS URL"`, `sp.acs_url`;
   - `id="saml-metadata-url"`, label `"SP metadata URL"`, `sp.entity_id`, description `"Some identity providers can import these settings from this URL."`;
   - `id="saml-start-url"`, label `"Start URL"`, `sp.start_url`, description `"Use it as the app's sign-on URL or tile link."`.
3. `saml-metadata-card.tsx`: `export function SamlMetadataCard({ saml }: { saml: Schema<"OrgSsoSaml"> })` renders a `SettingsCard` with title `"Identity provider metadata"` and no description:
   - `ReadOnlyCodeField`s: `id="saml-idp-entity-id"` label `"IdP entity ID"`, and `id="saml-sso-url"` label `"Sign-on URL"`.
   - When `metadata_url`: `ReadOnlyCodeField` `id="saml-idp-metadata-url"` label `"Metadata URL"`. Below it a line:
     - with `refresh_error`: `<StatusPill tone="warn">Refresh failed</StatusPill>` followed by `refreshErrorMessage(refresh_error)`;
     - else, when `refreshed_at`: `"Refreshed "` + `<RelativeTime value={refreshed_at} />` + `". It is checked once a day."`.
   - When `certificates_changed_at`: `<p role="note">Certificates changed on <TimeText value={…} style="datetime" />.</p>`.
   - A list (`<ul aria-label="Trusted certificates">`), one `<li>` per certificate with:
     - `SHA-256 ` plus the first 16 hex characters and `…` in a `<code>` with the full hash as `title`;
     - a `StatusPill` from `certificateState(c, Date.now())`:
       - `unreadable` → tone `error`, "Unreadable";
       - `expired` → tone `error`, "Expired";
       - `expiring` → tone `warn`, `` `Expires in ${days} day${days === 1 ? "" : "s"}` ``;
       - `valid` → tone `ok`, "Valid", followed by " until " and `<TimeText value={c.not_after} style="datetime" />`.
   - Under the list: `<p className="text-caption text-muted-foreground">An expired certificate still verifies sign-ins. Add the new certificate before your identity provider switches to it.</p>`.
4. `saml-provider-form.tsx`:
   ```tsx
   export function SamlProviderForm({ org, sso, locked }: {
     org: string;
     sso: Schema<"OrgSso">;
     /** `org_sso` or `saml` is missing: saving is locked (D-SA-4). */
     locked: boolean;
   })
   ```
   State, all `useState`:
   - `displayName`: initial `provider?.display_name ?? ""`;
   - `jitRole`: `provider?.jit_role ?? "viewer"`;
   - `source`: `"current" | "url" | "xml" | "manual"`, initial `"current"` when the saved provider is SAML, else `"url"`;
   - `metadataUrl`: initial `saml?.metadata_url ?? ""`;
   - `metadataXml`, `entityId`, `ssoUrl`, `certificates` (text): initial `""`;
   - `attributes`: four strings, initial from `saml?.attributes`, or `""`;
   - `saved`, `submitted`, `confirmOpen`.

   Here `provider = sso.provider` and `saml = provider?.protocol === "saml" ? provider.saml : undefined`.

   Layout, inside one `SettingsFormCard`:
   - title `"Identity provider"`;
   - description `"One SAML 2.0 identity provider for this organization."`;
   - `ariaLabel` = the title;
   - `disabledReason = locked ? "Needs an enterprise licence that includes SAML" : undefined`.

   Fields, in order:
   1. `TextField id="saml-display-name" label="Display name"`, required (the client message `"Enter a display name."`), error `fields.display_name`.
   2. A `Tabs` with value `source`, whose `TabsList` carries `aria-label="Metadata source"`. Triggers:
      - `"Current metadata"` (only when `saml` is defined);
      - `"Metadata URL"`, `"Metadata XML"`, `"Enter manually"`.

      Contents:
      - **current:** the text `"The stored metadata is kept. Choose another tab to replace it."`.
      - **url:** `TextField id="saml-metadata-url-input" label="Metadata URL" placeholder="https://idp.example.com/app/…/sso/saml/metadata"`, description `"cetra loads it now and checks it again once a day for new certificates."`, error `fields.metadata_url`.
      - **xml:** `Label htmlFor="saml-metadata-xml"` `"Metadata XML"` + `Textarea id="saml-metadata-xml"`, `aria-invalid` when `fields.metadata_xml`, error paragraph `id="saml-metadata-xml-error"` as in `text-field.tsx`. Description: `"Paste the XML your identity provider shows for this app. It is not refreshed."`.
      - **manual:** `TextField id="saml-idp-entity-id-input" label="IdP entity ID"` (error `fields.idp_entity_id`); `TextField id="saml-sso-url-input" label="Sign-on URL"`, description `"The identity provider's SSO URL for the HTTP-Redirect binding."` (error `fields.sso_url`); `Label` + `Textarea id="saml-certificates"` `"Signing certificates"`, description `"PEM, one or more. Up to 4."` (error `fields.certificates`).

      A `fields.metadata` error renders as a `<p role="alert">` under the tabs.
   3. The JIT role select: copy the OIDC section's Select, its labels and its help text exactly, with `id="saml-jit-role"`.
   4. A sub-heading `<h3>` `"Attribute names"`. Each attribute field's error is its own key (`fields["attributes.email"]`, `fields["attributes.display_name"]`, `fields["attributes.first_name"]`, `fields["attributes.last_name"]`). The form-level error shows also when `fields` names a key the form does not render, so no error is ever lost with the help `"Leave blank to use the usual names of Okta, Microsoft Entra ID, Google and Keycloak."`. Then four `TextField`s:
      - `saml-attr-email` `"Email attribute"`, placeholder `"email"`, error `fields["attributes.email"]`;
      - `saml-attr-display-name` `"Display name attribute"`, placeholder `"displayName"`;
      - `saml-attr-first-name` `"First name attribute"`, placeholder `"firstName"`;
      - `saml-attr-last-name` `"Last name attribute"`, placeholder `"lastName"`.

   Required-field rules (client side, shown after submit or touch, as in `provider-section.tsx`):
   - display name non-blank;
   - `source === "url"` → URL non-blank (`"Enter the metadata URL."`);
   - `"xml"` → XML non-blank (`"Paste the metadata XML."`);
   - `"manual"` → all three non-blank (`"Enter the IdP entity ID."`, `"Enter the sign-on URL."`, `"Paste at least one certificate."`).

   `dirty` is false while `locked`. Otherwise it is true when any of these hold:
   - the display name, JIT role or any attribute differs from the saved values;
   - `source` is not `"current"` (choosing a source tab is an explicit re-import, D-SA-11; Save then also re-fetches an unchanged metadata URL, which is how an owner accepts a refresh failure);
   - no SAML provider is saved yet (the form always saves; an unlocked, unsaved SAML form therefore arms the unsaved-changes guard, as intended).

   A re-import may change the provider's `created_at` (M2 writes a new row id only on insert, but treat it as possible), so "Saved" is guaranteed only for settings-only saves.

   Every input, tab trigger and the Select are `disabled={locked || save.isPending}`.

   The body built on submit (`Schema<"PutOrgSamlProvider">`):
   - `{ display_name: displayName.trim(), jit_role: jitRole }`;
   - plus by source: `url` → `metadata_url: metadataUrl.trim()`; `xml` → `metadata_xml: metadataXml`; `manual` → `idp_entity_id`, `sso_url` (trimmed) and `certificates: splitCertificates(certificates)`; `current` → no source key;
   - plus `attributes: attributesBody(attributes)` only when it has at least one key;
   - plus `confirm_entity_id_change: true` only on the confirmed retry.

   No key is ever sent as `undefined` or `""`.

   Mutation: `$api.useMutation("put", "/api/v1/orgs/{org}/sso/saml")`.
   - On success: clear the XML, manual and certificate inputs, set `source` to `"current"`, `setSaved(true)`, `storeOrgSso(queryClient, org, answer)`.
   - The body is built once per submit and kept in state (`pendingBody`), so the retry sends exactly the same body.
   - On error, in the first mutation's `onError`: if `problemOf(error)` (from `@/api/problem`) has status 409 and detail equal to `ENTITY_ID_CHANGE_DETAIL`, set `confirmOpen` to true instead of showing the error. While `confirmOpen` is true, `saveError` is `undefined`, so nothing shows behind the dialog. The retry runs through a second mutation, `const confirm = $api.useMutation("put", "/api/v1/orgs/{org}/sso/saml")`. The dialog:
     - `destructive`;
     - title `"Change the identity provider?"`;
     - description `"The new metadata names another entity ID. Saving it deletes the identities linked to the current one; members link again at their next sign-in."`;
     - confirm label `"Save and relink"`.

     - `details`: `` `Current IdP entity ID: ${saml?.idp_entity_id}` `` when a SAML provider is saved;
     - `pending={confirm.isPending}`, and `error={confirm.error ? problemMessage(confirm.error, "Could not save the identity provider.") : undefined}`.

     Confirming calls `confirm.mutate` with `{ ...pendingBody, confirm_entity_id_change: true }`. Its success runs the same success steps as a save and closes the dialog (`setConfirmOpen(false)`). Cancel closes it and calls `save.reset()` and `confirm.reset()`.
   - Any other non-field error shows through `SettingsFormCard`'s `error` (`problemMessage(save.error, "Could not save the identity provider.")`).
5. `provider-section.tsx`:
   - A protocol choice above the forms. A `RadioGroup` labelled `"Protocol"` (`aria-label="Protocol"`) with items `oidc` → `"OpenID Connect"` and `saml` → `"SAML 2.0"`, following the `create-token-dialog.tsx` pattern. Its state `protocol` starts at `sso.provider?.protocol ?? "oidc"`.
   - When a provider is saved and `protocol !== sso.provider.protocol`, show `<p role="note">` with:
     - `"Saving replaces the current provider. Members link again at their next sign-in, and the provider must be tested and enabled again."`;
     - and when `sso.provider.enforced`, instead: `"Turn off enforcement before switching the protocol."`.
   - `protocol === "oidc"` renders the existing OIDC form card and the "Callback URL" card, unchanged.
   - `protocol === "saml"` renders `<SamlProviderForm org={org} sso={sso} locked={samlLocked} />` and `<SamlSpCard sp={sso.saml_sp} />`.
   - The section takes a new prop `samlLocked: boolean`. When `protocol === "saml" && samlLocked && !locked` (with `org_sso` locked the page already shows its lock), render `<LockedFeature feature="saml" upgradeUrl={upgradeUrl} />` above the SAML form (the section takes `upgradeUrl` too), whatever the saved provider is.
   - Switching the radio unmounts the form being left; its unsaved edits are dropped, which the switch note's warning covers.
   - The radio items are `id="sso-protocol-oidc"` and `id="sso-protocol-saml"`; `onValueChange` narrows the string with `value === "saml" ? "saml" : "oidc"`.
   - The OIDC form's client secret is required, and its placeholder "Stored; leave blank to keep it" is shown, only when `provider?.protocol === "oidc"`. Today the check is `provider === undefined`. A switch from SAML needs a secret (M2 SA2.4).
   - The existing OIDC form's title and description stay as they are; the radio sits above it.
   - `ProviderRemoval` takes `confirmText: string` instead of `issuer` (the type-to-confirm text and label use it).
6. `org-sso-settings.tsx`:
   - `const samlLocked = locked || entitlements.locked("saml");`.
   - `ProviderSection` gets `samlLocked` and its `key` becomes `` `${sso.provider?.created_at ?? "no-provider"}-${sso.provider?.protocol ?? ""}` ``, so a protocol switch remounts the forms.
   - When `sso.provider?.protocol === "saml" && sso.provider.saml`, render `<SamlMetadataCard saml={sso.provider.saml} />` between the provider section and the domains section.
   - `StatusSection`'s `locked` becomes `locked || (sso.provider?.protocol === "saml" && entitlements.locked("saml"))`.
   - `ProviderRemoval` gets `confirmText={sso.provider.issuer ?? sso.provider.saml?.idp_entity_id ?? sso.provider.display_name}`. This replaces M2's temporary `issuer ?? display_name`.
   - Pass `upgradeUrl={entitlements.upgradeUrl}` to `ProviderSection`, which shows the SAML lock (step 5).
7. `sso.test.tsx`: no behaviour change. Update only what the protocol radio breaks: the first test also asserts the radio `OpenID Connect` is checked. Keep every other assertion.
8. `sso.saml.test.tsx` (new, same helpers as `sso.test.tsx`; copy `meAs` and an `openPage({ features })` that sets the entitlements features to the given list):
   - Constants: a typed `const samlSso: Schema<"OrgSso">` built from the fixture with `provider: { protocol: "saml", display_name: "Acme SAML", enabled: false, enforced: false, jit_role: "viewer", created_at: <fixture's>, updated_at: <fixture's>, saml: { idp_entity_id: "https://idp.acme.test/metadata", sso_url: "https://idp.acme.test/sso", metadata_url: "https://idp.acme.test/metadata", certificates: [{ sha256: "a".repeat(64), not_before: "2025-01-01T00:00:00Z", not_after: "2099-01-01T00:00:00Z" }], attributes: {}, refreshed_at: "2026-01-01T00:00:00Z", refresh_error: null, certificates_changed_at: null } }` (adjust `null` versus omitted to the generated types).
   - Tests:
     1. `"sets up a SAML provider from a metadata URL"`: OIDC fixture, features `org_sso` and `saml`. Choose `SAML 2.0`, clear and type the display name, type the URL, save. The handler records the body → `toEqual({ display_name: "Acme SAML", jit_role: "viewer", metadata_url: "https://idp.acme.test/metadata" })`; answer `samlSso`. The protocol change remounts the forms, so assert the "Identity provider metadata" card and the "Trusted certificates" list, not "Saved".
     2. `"warns before switching protocol"`: OIDC fixture; choosing `SAML 2.0` shows the switch note. With `enforced: true`, it shows the enforcement note.
     3. `"shows the service provider details"`: the values by their `<code>` ids (`#saml-entity-id`, `#saml-acs-url`, `#saml-metadata-url`, `#saml-start-url`; the entity ID and SP metadata URL share a value, so do not use `getByText`) and their copy buttons (`Copy acs url`, etc.).
     4. `"sends pasted XML and manual fields"`: XML pasted with `user.click` on the textarea then `user.paste` → `metadata_xml`. Manual, with two PEM blocks pasted into the textarea (`getByLabelText("Signing certificates")`) → `certificates` has 2 entries, plus `idp_entity_id` and `sso_url`.
     5. `"keeps the current metadata when only the name changes"`: `samlSso`, change the display name → body `{ display_name, jit_role }`, with no source keys. The handler answers `samlSso` with the new `display_name` (same `created_at`), then "Saved" shows.
     5b. `"re-imports an unchanged metadata URL"`: `samlSso` with `refresh_error: "entity_id_changed"`; open the tab `getByRole("tab", { name: "Metadata URL" })` without editing; Save is enabled and sends `metadata_url` equal to the stored one.
     5c. `"switching from SAML to OIDC asks for a client secret"`: `samlSso`; choose `OpenID Connect`, fill the issuer and client ID, save without a secret → the client message "Enter a client secret." and no request.
     6. `"sends only the attribute names that are set"`: email attribute `mail` → `attributes: { email: "mail" }`.
     7. `"confirms an entity ID change"`: the first PUT answers `problemResponse(409, "conflict", { detail: ENTITY_ID_CHANGE_DETAIL })`. The dialog shows; confirming sends the second body with `confirm_entity_id_change: true`. A second run that presses Cancel shows no error in the form afterwards.
     8. `"shows field errors under their fields"`: a 400 with `metadata_url` and `attributes.email` errors → the texts and `aria-invalid`.
     9. `"shows a metadata source error under the tabs"`: a 400 with field `metadata` → the `alert`.
     10. `"shows certificate expiry and refresh failures"`: certificates expiring in 10 days (computed from `Date.now()`), expired, and epoch-unreadable, plus `refresh_error: "entity_id_changed"` and `certificates_changed_at` → the three pills, the message and the note.
     11. `"locks SAML without the saml feature and keeps turning it off"`: features `org_sso` only, `samlSso` with `enabled: true`. The inputs are disabled; the "Locked feature" note for SAML shown; the Enabled switch can still be turned off (the PATCH handler receives `{ enabled: false }`).
     11b. `"shows the SAML lock when choosing SAML over OIDC"`: OIDC fixture, features `org_sso` only; choosing `SAML 2.0` shows the SAML "Locked feature" note.
     12. `"removal asks for the IdP entity ID"`: the remove dialog's input label is `Type "https://idp.acme.test/metadata" to confirm` (query the dialog's input by that label, not by text, since the metadata card shows the same value).
     13. `"asks before leaving with unsaved SAML changes"`: copy `asks before leaving the provider form with unsaved changes` from `sso.test.tsx`, with `samlSso` and typing in `Display name`.

   Every `server.use` that overrides `GET /api/v1/orgs/:org/sso` runs before `openPage`, so the first render already sees it.

   Time-dependent assertions use `vi.useFakeTimers({ toFake: ["Date"] })` with `vi.setSystemTime(new Date("2026-01-01T00:00:00Z"))`, or compute from `Date.now()`. Pick the approach `sso.test.tsx` or nearby tests already use; if none, compute from `Date.now()`.

9. `saml.test.ts` (new, vitest, no rendering):
   - `certificateState` at exactly 30 and 31 days before expiry (`expiring` with 30 days, then `valid`), expired, and the epoch case (`unreadable`);
   - `splitCertificates` with two blocks, text before the first block (dropped), no BEGIN marker (one entry), and blank input (`[]`);
   - `refreshErrorMessage` for all six values;
   - `attributesBody` dropping blanks and trimming.

**Done when:** written and committed.

**Commit:** `Set up SAML providers on the single sign-on page`

---

## SA3.3 — Docs and spec

**Goal:** the user docs and spec 11 describe SAML.

**Edit:** `website/src/content/docs/docs/platform/sso.mdx`, `website/src/content/docs/docs/platform/plans-and-licence.mdx`, `website/src/content/docs/docs/self-hosting/authentication.mdx`, `docs/spec/11-ui.md`, `CHANGELOG.md`.

**Steps:**
1. `sso.mdx`:
   - Frontmatter description: `The ways members sign in to cetra, and how an org can require OIDC or SAML single sign-on for its own email domains.` (under 160 characters).
   - Intro first sentence becomes: `cetra supports password sign-in, GitHub, Google and any OpenID Connect (OIDC) provider for the whole instance, and OIDC or SAML 2.0 for an org's own provider.`
   - In "What members see", the error-code sentence lists `saml_invalid` and `saml_unsupported` too.
   - In "Instance providers", add at its end: `Instance providers use OIDC. SAML is available only for an org's own provider.`
   - "Per-org SSO" first paragraph: `An org owner registers one identity provider for the org, using OIDC or SAML 2.0, and proves ownership of the org's email domains with a DNS TXT record:`. Remove the "part of the Teams add-on" sentence and add, after the steps: `Per-org SSO needs a licence or plan that includes it. SAML needs an enterprise licence that includes SAML, and the instance's public URL must use https.`
   - A new section `## Set up SAML`, placed after the whole "Per-org SSO" section (which keeps its break-glass paragraph). It contains:
     1. A one-sentence lead: `To use SAML, create a SAML app at your identity provider for cetra, then give cetra the app's metadata.`
     2. Numbered steps:
        1. `On the Single sign-on page, choose **SAML 2.0**. The **Service provider details** card shows the values your identity provider asks for.`
        2. `In your identity provider, create a SAML 2.0 app and assign the people or groups who should sign in. Set the ACS URL (also called the single sign-on URL or reply URL) and the entity ID (also called the audience) to the values cetra shows. Send the email address in an attribute named email, or use it as the NameID with the email address format.`
        3. `Copy the app's metadata URL into **Metadata URL**, or paste the metadata XML. You can also enter the entity ID, sign-on URL and signing certificate yourself.`
        4. `Save, then test the sign-in and enable the provider as for OIDC. To show cetra as a tile in your identity provider, use the **Start URL** as the app's sign-on URL.`
     2b. A paragraph: `Switching an org's provider between OIDC and SAML deletes the links between members and the old provider, turns the provider off until you test it again, and is refused while single sign-on is enforced.`
     3. A subsection `### Okta` with steps: create the app integration with sign-in method **SAML 2.0**; single sign-on URL = the ACS URL; audience URI = the entity ID; name ID format **EmailAddress**, application username **Email**; assign the people or groups on the **Assignments** tab; then copy the **Metadata URL** from the app's **Sign On** tab into cetra.
     4. A subsection `### Keycloak` with steps: create a client with client type **SAML** and client ID = the entity ID; valid redirect URIs and **Assertion Consumer Service POST Binding URL** = the ACS URL; name ID format **email** with **Force name ID format** on; turn **Client signature required** off (cetra does not sign its requests); leave **Encrypt assertions** off; the metadata URL is `https://<KEYCLOAK_HOST>/realms/<REALM>/protocol/saml/descriptor` (it must use https).
     5. A subsection `### What cetra accepts`, a short list:
        - signed responses or assertions with SHA-256 or stronger;
        - one assertion per response;
        - no encrypted assertions;
        - sign-in started from cetra; a click on the app tile in the identity provider starts a new sign-in from cetra;
        - no single logout;
        - the email and name attributes Okta, Microsoft Entra ID, Google and Keycloak send by default, or the names the owner sets.
     6. A subsection `### Certificates and metadata`:
        - `When you give a metadata URL, cetra checks it once a day and trusts every signing certificate it lists, up to four, so your identity provider can add the next certificate before it switches. Pasted metadata is not refreshed; paste it again when the certificate changes. The page warns 30 days before a certificate expires. If the metadata at the URL names another entity ID, cetra keeps the old metadata and shows the error until you save it again. Changing the entity ID deletes the links between members and the old provider; members link again at their next sign-in.`
   - The final sentence of "Per-org SSO", `SAML is not supported.`, is removed (the new "Instance providers" sentence covers instance providers).
2. `plans-and-licence.mdx`: after the `org_sso` row, add `| \`saml\` | SAML 2.0 for an org's own provider. See [Single sign-on](/docs/platform/sso/#set-up-saml). |`.
3. `authentication.mdx`: check that line 126 (the "Teams add-on" mention) is still accurate, and leave it if so. Line 91: replace `SAML is not supported.` with `Instance providers use OIDC; an org can use SAML for its own provider ([Single sign-on](/docs/platform/sso/#set-up-saml)).`
4. `docs/spec/11-ui.md`: replace the sentence SA2.8 added to the Single sign-on settings bullet ("SAML providers are configured on the same page (M3 of track SA).") with:
   - a description of the protocol radio;
   - the SAML form's fields and the metadata source tabs;
   - the Service provider details card, the Identity provider metadata card (certificates, refresh status) and the entity-ID confirmation;
   - the SAML lock (D-SA-4).
5. `CHANGELOG.md` § `[Unreleased]` (this is the track's only CHANGELOG line; M4 does not add another): append to the existing `### Added` list (create it before `### Changed` only if it is missing) the line `- Org owners can set up SAML 2.0 single sign-on for their org, with metadata import, automatic certificate rotation from a metadata URL, and the same domains and enforcement as OIDC (the \`saml\` licence feature).`

**Done when:** written and committed.

**Commit:** `Document SAML single sign-on`

---

## SA3.4 — Checks and review (coordinating session)

1. `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`, then `just check-web` and `just check-docs`. Fix failures through Haiku fix phases.
2. A Sonnet review of the M3 diff against this plan and against the API (SA2.4): bodies sent, error handling, locks.
3. Roadmap status "M3 implemented"; update the PR body.
