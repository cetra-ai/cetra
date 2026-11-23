# Track SA — SAML 2.0 single sign-on

Design doc. The user reviews it before any plan is written. Each milestone gets its own plan, written from the code that exists when the milestone starts (`roadmap.md` § How the work is organised).

**Status:** design approved by the user on 2026-11-23. M1's plan: [`01-m1-crate.md`](01-m1-crate.md).

**Decisions:** `D-SA-1` to `D-SA-19`, in `docs/mvp-decisions.md` § Track SA. "(user)" marks the user's answers of 2026-11-23.

**Research:** the crate survey and the competitor comparison of 2026-11-23 are summarised in § Library and § Competitors. Versions were looked up that day; each plan looks them up again.

## Why

On 2026-11-23 the user decided that the self-hosted licence ($5,400 a year) should compete with enterprise tiers priced $20k–$60k+ a year. Today the licence unlocks per-org OIDC SSO, SSO enforcement and project roles. It now also covers support (D197), audit logs and SCIM (a separate track), and SAML (this track).

SAML has been out of the MVP since D20. Per-org SSO (track P M5, D-P-25, D-P-51–D-P-65) speaks only OIDC. Many enterprise buyers still run SAML-only IdP setups: ADFS, older Okta and Entra apps, and security teams with a "SAML or nothing" rule. This track amends D20 (D-SA-1).

## Goals

- **SP-initiated SAML 2.0 login per org**, as an alternative to the org's OIDC provider. An org has one provider, and its owner picks OIDC or SAML for it (D-SA-2).
- **Everything else stays as org SSO works today:**
  - DNS-verified domains (D-P-54, D-P-72);
  - email-first discovery (D-P-60);
  - disabled until an owner's test sign-in succeeds, then enabled, then enforced (D-P-57, D-P-58);
  - JIT membership within seats (D-P-56);
  - lapse rules (D-P-8, D-P-61);
  - break-glass (D-P-63).
- **IdP metadata import** from a URL (refreshed daily) or pasted XML. Manual fields are a fallback (D-SA-11).
- **Certificate rotation without downtime:** every signing certificate in the metadata is accepted, up to 4 (metadata with more is refused), so an IdP can publish its next certificate before it switches (D-SA-11).
- **Attribute mapping** of email and display name. The attribute names are configurable and have defaults. New members get the role of their pending invitation, or else the org's `jit_role`, exactly as with OIDC (D-SA-6).
- **A security contract for response validation** that defends against the known attack classes: signature wrapping, comment injection, XXE and DTDs, replay, login CSRF, and algorithm downgrade (D-SA-9). It is backed by a hostile test corpus and a fuzz target (D-SA-15).
- **Gating:**
  - a new `ee` feature `saml` (D-SA-4);
  - on self-hosted instances, the licence grants it;
  - on the cloud, the control plane federates SAML for Enterprise orgs only (D-SA-3).
- **A shared crate, `cetra-saml`,** under the Elastic-2.0 licence. It has no workspace dependency, so the server and the control plane (by git) use the same validation code (D-SA-3, D-SA-8).

## Non-goals

| Not in this track | Why / where it goes |
|---|---|
| IdP-initiated (unsolicited) login | Refused: it is open to replay and login CSRF. IdP tiles still work: an unsolicited response is thrown away unread, and the browser restarts an SP-initiated login (D-SA-5). |
| Several providers per org, or OIDC and SAML at the same time | One provider per org (D-SA-2). |
| Instance-wide SAML (`[[auth.saml]]` in `cetra.toml`) | Per-org only (D-SA-7). A self-hosted operator sets SAML up on their org. |
| Encrypted assertions, signed AuthnRequests | Not at first (D-SA-8). The SP holds no private key. They can be added when a customer needs them. |
| Group or role mapping from SAML attributes | Group-driven roles belong to SCIM, in the audit logs and SCIM track, for OIDC and SAML alike (D-SA-6). |
| Single Logout, Artifact binding, ECP, ForceAuthn and other protocol options | Not needed for login. A cetra logout ends the cetra session only, as with OIDC (D-SA-10). |
| External (paid) security review | Internal review only (D-SA-15, user). |
| Control-plane SAML federation | Track Q's code, in the private repository. This track delivers the crate and a request to Q (§ Cloud). |
| Marketing-site claims | The site and docs claim SAML only once M4 is done (D197). |

## Competitors

Read in `/home/sebastien/github` on 2026-11-23, for comparison only.

| Product | SAML | Shape |
|---|---|---|
| LangWatch | Enterprise, in progress (ADR-124) | Per-org connections of type `oidc` or `saml`, several per org, DNS-verified domains, identifier-first sign-in. Built on `@better-auth/sso`, which uses the JS library samlify. Two alternatives were rejected: BoxyHQ Jackson, because it adds a second process and datastore, and an in-house implementation. Whether to support IdP-initiated login is still open. |
| Opik | Enterprise only, closed source | One configuration per org. The IdP entity ID, SSO URL and certificate are pasted in; there is no metadata import. Attributes are `guid`, `email` and optional names, with an optional workspace sync from groups. SP-initiated only. |
| Langfuse | None in code | Per-org OIDC only (`multi-tenant-sso`). SAML appears only as a marketing keyword. |
| Phoenix | None | Points to the paid Arize AX for SAML. |
| Helicone | None in code | An Enterprise row on the pricing page. |
| Laminar, MLflow | None | — |

Every competitor that ships SAML gates it to Enterprise, and none turns IdP-initiated login on by default. They build on decade-old libraries from their own ecosystems: samlify, or OpenSAML on the JVM. Rust has no library like that (§ Library).

## Architecture

```
                 browser
   ┌───────────────┼──────────────────────────────────────────┐
   │ 1. /login: email first → POST /api/auth/sso/discover      │
   │ 2. GET /api/auth/sso/authorize?domain=  (unchanged entry) │
   ▼                                                           │
 cetra-server ── ee/org_sso ───────────────┐                   │
   │ routing index (domain → org, flags,    │                   │
   │ built provider: Oidc | Saml)           │                   │
   │                                        │                   │
   │ Saml: cetra-saml::authn_request ───────┼─► 302 to IdP SSO URL (HTTP-Redirect,
   │       pending request in memory        │     unsigned AuthnRequest + RelayState)
   │       + binding cookie (SameSite=None) │                   │
   │                                        │   IdP authenticates the user
   │ POST /api/auth/saml/{org_id}/acs ◄─────┼── auto-submitted form (HTTP-POST binding)
   │   cetra-saml::validate_response        │                   │
   │   → Assertion facts (subject, email,   │                   │
   │     name, session index, times)        │                   │
   │   → the same sign-in core as OIDC      │                   │
   │     (D-P-55 linking, D-P-56 JIT,       │                   │
   │      D-P-57 test, D-P-59 session)      │                   │
   └────────────────────────────────────────┘                   │
                                                                │
 crates/saml (cetra-saml, Elastic-2.0, no workspace deps) ◄─────┘
   metadata parse · AuthnRequest · response validation · test IdP (dev only)
   on bergshamra (XML parser, exc-c14n, XML-DSig verify)

 control plane (private repo, track Q) ── uses cetra-saml by git, as it uses cetra-token
```

### Parts

1. **`crates/saml` (`cetra-saml`).** A library crate under the Elastic-2.0 licence, like `cetra-token` (D-P-26). It depends on no workspace crate and knows nothing of HTTP, `meta.db` or orgs, so the control plane can use it by git commit. It is pure functions over bytes and explicit inputs (the clock, the expected values). Its public surface is the only contract the server and the control plane rely on:
   - **Parse IdP metadata.** Input: XML bytes. Output: the IdP entity ID, the SSO URL for the HTTP-Redirect binding, the signing certificates (at most 4, D-SA-11), and each certificate's validity dates for display. Also refuses metadata it cannot use: no Redirect SSO endpoint, no signing key, or a DOCTYPE.
   - **Build an AuthnRequest.** Input: the SP entity ID, the ACS URL, the IdP SSO URL and a fresh request ID. Output: the redirect URL (DEFLATE, base64 and URL encoding, with `RelayState`). Unsigned (D-SA-8).
   - **Validate a Response.** Input: the base64 POST body, the expected values (the SP entity ID, the ACS URL, the IdP entity ID, the trusted certificates, the request ID it must answer, now, the allowed skew) and the attribute names to read. Output: the facts of the one Assertion (NameID and its format, email, display name), or a typed error. The full rule set is D-SA-9.
   - **Tell whether a Response is solicited.** Reads only `InResponseTo`, so the server can treat an unsolicited response as a restart (D-SA-5) without trusting anything in it.
   - **A test IdP**, behind a dev-only feature. It signs Responses with a fixture key and can build every malformed and hostile variant the corpus needs (D-SA-15). Never compiled into release builds.
   - **Errors** are a `thiserror` enum whose messages carry no payload: no NameID, email, attribute value or certificate (`AGENTS.md` § Privacy).

2. **The server side (`crates/server/src/ee/org_sso/`).** The org's provider becomes OIDC or SAML. The routing index (D-P-62) holds a built provider of either kind. Everything the protocol does not change is shared:
   - domains;
   - discovery;
   - the test sign-in;
   - linking;
   - JIT;
   - enforcement;
   - sessions;
   - break-glass;
   - lapses.

   SAML adds:
   - the stored SAML configuration (the metadata source, the parsed metadata, the attribute names);
   - the per-org SP endpoints (D-SA-12);
   - the in-memory pending-request table and the browser-binding cookie (D-SA-10);
   - the daily metadata refresh (D-SA-11);
   - a shared sign-in core that M2 extracts from `org_sso::login::finish`. Today that one function opens the OIDC state cookie, exchanges the code, checks the claims and the domain, and calls `admit`. M2 splits it so the OIDC callback and the SAML ACS both end in one function.
     - Its inputs are the org, the subject, the email, the display name, the test flag, the test owner (D-P-57) and the redirect path.
     - The email normalisation, the verified-domain check (D-P-55), linking, JIT, seats, and the session with its `sso_org_id` stay inside it, so neither protocol can skip them.
     - Only the protocol's own checks run in front of it: OIDC's `email_verified`, and SAML's D-SA-9.

3. **The API and the web UI (`web/src/ee/org-sso/`).**
   - The org SSO settings page gains a protocol choice. The SAML form takes a metadata URL or pasted metadata XML, or manual fields, plus the attribute names.
   - It shows the values to copy into the IdP: the SP entity ID, the ACS URL, the SP metadata URL and the tile start URL.
   - It shows the trusted certificates with their expiry dates, and the last refresh with any error.
   - The Status and Domains sections are unchanged.

4. **The control plane (track Q).** It federates SAML for Enterprise orgs (D-SA-3). Q owns its own design. This track hands Q the crate and a written request (§ Cloud).

### Contracts between the parts

- **Crate → server, crate → Q:** the four operations above, the facts type and the error enum. A change to them is a breaking change of `cetra-saml` and is made in both consumers in step (`AGENTS.md` § No compatibility layers; Q moves its git pin).
- **Server ↔ IdP:**
  - SP entity ID `<public_url>/api/auth/saml/<org_id>`, which also serves the SP metadata XML;
  - ACS `<public_url>/api/auth/saml/<org_id>/acs`, HTTP-POST binding;
  - AuthnRequest over HTTP-Redirect;
  - NameID format requested: `unspecified`;
  - no signing or encryption key in the SP metadata (D-SA-12).
- **Server ↔ browser:**
  - the entry stays `GET /api/auth/sso/authorize?domain=&redirect=` (D-P-60);
  - the ACS answers with a redirect to the saved relative path, or to `/login?error=<code>`;
  - new login error codes: `saml_invalid` (a response failed validation; details go only to the server log, by error class) and `saml_unsupported` (an encrypted assertion, or a feature this SP refuses);
  - the ACS is exempt from D141's `Origin` check, because the IdP's POST comes from the IdP's origin. The exemption covers exactly `POST /api/auth/saml/{org_id}/acs`, not the `/api/auth/saml/` prefix. The binding cookie and the signature protect it instead (D-SA-10);
  - the ACS body is `application/x-www-form-urlencoded` (`SAMLResponse`, `RelayState`). It is read by an extractor of its own with a 256 KiB body cap, an exception to D141's JSON-only rule;
  - a session cookie already present at the ACS is ignored and replaced by the new session, as in the OIDC callback.
- **Entitlements:** feature `saml` (D-SA-4). Setting up, changing or enabling a SAML provider needs `saml` and `org_sso`. Lapses follow D-P-61 for each feature.
- **Storage (D-SA-17):** the provider row (`org_sso_providers`) keeps the flags for both protocols and gains a `protocol` column. Its OIDC columns become nullable. The SAML configuration is in `org_sso_saml`, keyed by the provider row, and its certificates in `org_sso_saml_certificates`. M2's migration rebuilds `org_sso_providers` as a pre-release exception (spec 04 § Format versions).

### Library (D-SA-8)

Looked up 2026-11-23 (`cargo info`, crates.io, the repositories):

| Option | What it is | Verdict |
|---|---|---|
| `samael` 0.0.22 (MIT) | The best-known Rust SAML crate | **Rejected.** By default it needs libxml2 and xmlsec1 (C), and `openssl` even without them. That breaks the static musl build and the no-OpenSSL rule. |
| `saml-rs` 0.8.0 (MIT) | The only complete pure-Rust SP and IdP | **Rejected.** First published 2026-06-14, one maintainer, unaudited, breaking releases every 1–2 weeks. Seven advisories it published itself in November 2026, one of them signature wrapping (GHSA-h96q-6vh9-c43w). It parses each message with two XML parsers (issue #629), which is the classic root of wrapping bypasses. Adds about 58 crates, some of them pre-release pins. |
| Fully in-house, with our own exc-c14n | `roxmltree`, our own canonicalisation, and the RustCrypto or ring primitives already in the lock | **Rejected.** A canonicalisation bug is a signature bypass, and we would have no interop corpus to test ours against. |
| A broker (Keycloak, Dex, BoxyHQ Jackson) in front of cetra's OIDC | No SAML code in cetra | **Rejected as the product answer:** it breaks the one-binary pitch. It stays documented as a workaround until M4 ships. |
| **Our own SP layer on `bergshamra` 0.12.0 (BSD-2-Clause)** | A pure-Rust XML parser, all W3C canonicalisations, XML-DSig and XML-Enc, tested against the full xmlsec1 interop suite. It always rejects duplicate IDs and has a strict reference mode. | **Chosen (user).** We use only its parser, exc-c14n and signature verification, with default features off (`rustcrypto` only, no `pkcs11`, `post-quantum` or `legacy-algorithms`). One parser sees each document. The checks bergshamra leaves to the caller (does the signature cover the node we read, exactly one Assertion, the binding checks) are ours, written to D-SA-9 and tested by the hostile corpus. |

bergshamra is young too: first released 2026-02-22, one main author. The mitigations:
- pin it exactly;
- read its verification path in M1;
- test it with our own hostile corpus and fuzz target;
- pass `cargo deny` (licence BSD-2-Clause, already allowed in `deny.toml`; bans; sources);
- run M4's internal review.

**Measured in M1 (2026-11-23):** `bergshamra` 0.12.0, `uppsala` 0.12.1 and `x509-cert` 0.2.5, with bergshamra's default features off and `rustcrypto` on. `cetra-saml`'s normal dependency tree has 298 entries. No C code and no OpenSSL. It brings the pre-release crates `ed448-goldilocks` 0.14.0-pre.15, `x448` 0.14.0-pre.12 and `slh-dsa` 0.2.0-rc.5 through kryptering, and second versions of `der`, `sha2`, `base64`, `rand` and `getrandom`. `cargo deny`'s bans, licences and sources pass (`multiple-versions` only warns). A Sonnet security review of the crate found no critical or high issue; its medium and low findings are fixed by SA1.9.

### Cloud

D183 and D-Q-60 make the control plane the only identity provider of cloud cells. Q federates Teams OIDC SSO, and cells never grant `org_sso`. SAML follows the same rule (D-SA-3, user):
- On the cloud, Q is the SAML SP for **Enterprise orgs only** (user). Teams stays OIDC.
- Q uses `cetra-saml` by git commit. Its routes, storage and UI are Q's own design, modelled on M2 and M3 as D-Q-60 was modelled on P's M5.
- Cells change nothing for the cloud.

At the end of M1, the request is written to the shared notes folder (`sa-requests-for-q.md`), as earlier tracks wrote theirs (`ao-requests-for-q.md`). It holds the crate's API, the security contract (D-SA-9, D-SA-10) and the gate (Enterprise orgs).

## Decisions

Each decision is also in `docs/mvp-decisions.md` § Track SA.

| # | Decision | Rejected alternatives |
|---|---|---|
| D-SA-1 | **SAML 2.0 SSO is in the product (user, amends D20 and D177).** Per-org SP-initiated SAML beside OIDC, for the self-hosted licence and cloud Enterprise. The site and docs claim it only once M4 is done (D197). | Keep SAML out and point customers at a broker. |
| D-SA-2 | **One provider per org: OIDC or SAML (user).** The org's single provider row has a protocol (storage: D-SA-17). Switching protocol replaces the provider's configuration: the org's identities are deleted (a new namespace of subjects, as for an issuer change in D-P-52), and `enabled` and `enforced` are reset to off, so the provider needs a new test sign-in. The display name, domains and `jit_role` are kept. The switch is refused while enforced (turn enforcement off first; D-P-57 already forbids deleting an enforced provider). The routing index (D-P-62) keeps a built SAML provider while its IdP entity ID, SSO URL, certificate set, attribute names, display name and `jit_role` are unchanged. | Several providers per org (per-domain routing, a change to the OIDC model too); one of each at once (only useful for a migration, at double the surface). |
| D-SA-3 | **On the cloud, the control plane federates SAML for Enterprise orgs (user, follows D183).** `cetra-saml` is shared with Q by git. Cells keep `cloud` as their only provider. | Cells as SAML SPs (breaks D183); self-hosted only; Teams on the cloud gets SAML too (the user keeps SAML for Enterprise). |
| D-SA-4 | **A new `ee` feature, `saml` (user, amends D-P-12's list).** An append-only name like the others. A SAML provider needs `saml` and `org_sso`. Licences sold with SAML list both; the $5,400 licence lists `saml` in keys issued from now on, and a key issued before needs reissuing to get it. Lapses follow D-P-8 and D-P-61: sign-in and enforcement keep applying, and setup, changes and enabling are `plan_required`. | Reuse `org_sso`, which would give SAML to every licence and could not be sold separately. |
| D-SA-5 | **SP-initiated only (user).** A Response must answer a pending AuthnRequest (D-SA-10).
- **Unsolicited means no `InResponseTo`.** The crate reads only that attribute. The rest of the response is thrown away unread.
- **The restart.** The browser is redirected to an internal org-keyed start: `GET /api/auth/saml/{org_id}/start`, which never takes a domain. It begins an SP-initiated login with the redirect path `/`. That is one hop, and nothing from the response is used.
- **Loop guard.** The start sets a short-lived marker cookie. A second unsolicited response within 2 minutes answers `saml_invalid` instead of restarting, for an IdP that never sets `InResponseTo`.
- **Disabled providers.** A disabled provider answers `sso_unavailable` at the restart, since its test sign-in needs an owner who started it from the settings page.
- **Unknown request IDs.** A response whose `InResponseTo` is unknown, expired or already consumed is `saml_invalid`, not a restart.
- **IdP tiles** work through the restart, or by pointing the tile at `/api/auth/sso/authorize?domain=<domain>`. | Unsolicited responses on per org (opt-in) or for all: replay within the validity window, and login CSRF into the attacker's account. |
| D-SA-6 | **Attributes: email and name, JIT as OIDC (user).** Email comes from a configurable attribute; if it is absent, from the NameID when its format is `emailAddress`. The display name comes from configurable attributes (a full name, or first plus last). The defaults cover Okta, Entra, Google and Keycloak's usual names. D-P-55 applies unchanged: verified domains only, no linking of instance admins. SAML has no `email_verified`; the assertion is trusted like an absent claim. New members get the role of their pending invitation, or else `jit_role`. | Group-to-role mapping at each login or at the first login. Group-driven roles come with SCIM, for both protocols. |
| D-SA-7 | **No instance-wide SAML (user).** | `[[auth.saml]]` under the licence: a second configuration path, docs and tests for what one org can do. |
| D-SA-8 | **No SP keys: unsigned AuthnRequests, no encrypted assertions (user, for encryption).** An `EncryptedAssertion` is refused with `saml_unsupported`, which the owner sees at the test sign-in. The library is our own SP layer on `bergshamra` 0.12.0 with default features off (user; § Library). | Optional per-org encryption keys; `samael`; `saml-rs`; in-house c14n; a broker. |
| D-SA-9 | **The response validation contract.** All of these are in `cetra-saml`, each with named tests.

**Parsing**
1. The decoded body is at most 256 KiB. A `DOCTYPE` or `ENTITY` declaration is refused before parsing. The parse is limited to a depth of 32, 10 000 elements and 64 attributes per element.
2. One parser: every read of the message uses bergshamra's parser (uppsala) with one configuration. No second XML parser ever reads it. The server parses twice with that same parser: once to read `InResponseTo` for the pending lookup (D-SA-10), once to validate.

**Shape**
3. `Version="2.0"` and status Success.
4. Exactly one `Assertion` and no `EncryptedAssertion`.
5. At most two `Signature` elements, one on the Response and one on the Assertion, each with exactly one `Reference`. Any Signature present must verify. Either one is enough when it covers the Assertion: a Response signature covers its only Assertion.

**Signatures**
6. Signatures are checked only against the org's trusted certificates. Each trusted key is tried in turn. `KeyInfo` in the message never adds a key: it may only reorder the trusted ones.
7. bergshamra's strict reference mode is used. The reference resolves, by node identity, to the element whose content is then read, and nothing is read from outside the signed element.
8. Transforms are only enveloped-signature and exclusive c14n, with or without comments.
9. Signature algorithms are RSA-SHA256/384/512 and ECDSA-SHA256/384, with digests SHA-256 and above. SHA-1 and MD5 are refused.

**Binding checks**
10. `Issuer` equals the IdP entity ID.
11. `Destination` (when present) and the bearer `SubjectConfirmationData` `Recipient` equal the ACS URL.
12. The Assertion has `Conditions` with an `AudienceRestriction` that lists the SP entity ID. Other audiences in it are ignored.
13. `InResponseTo` is required on the Response and on the bearer confirmation data, and both equal the pending request ID. A Response without it is unsolicited and never reaches validation (D-SA-5).
14. At least one bearer `SubjectConfirmation` passes every check. Other confirmations are ignored.

**Times** (60 s skew for every check)
15. `IssueInstant` is no more than 5 minutes old and not in the future.
16. `NotBefore` and `NotOnOrAfter` hold.
17. The bearer confirmation's `NotOnOrAfter` holds.

**Values**
18. The NameID and every attribute value are read as the concatenation of all their text. A comment or processing instruction inside them is refused (the CVE-2017-11427 class).
19. A `transient` NameID is refused, because it cannot identify the person again. | Leniency for IdP quirks: SHA-1 for old ADFS, unsigned assertions inside a signed Response that are not its only child. Each would reopen a known bypass. |
| D-SA-10 | **Pending requests in memory, bound to the browser.**

**The entry.** Authorize records the request ID in memory, with:
- the org;
- the test flag and the test owner (D-P-57);
- the redirect path, checked by `state_cookie::safe_redirect`;
- the SHA-256 of a random binder;
- the client IP;
- an expiry 10 minutes later.

The table lives in the one server process; a single binary shares it with nothing.

**Bounds.** At most 50 pending entries per client IP and 1 000 per org. A new entry evicts the oldest one of its IP, or of its org, when either is full. The start also shares D-P-60's per-IP bucket. An attacker therefore cannot fill an org's table from a few addresses without pushing out only their own entries first.

**The binder cookie.** The binder travels in a cookie `HttpOnly; Secure; SameSite=None` whose path is the org's ACS path, so two orgs' logins in one browser do not clash. It is needed because the IdP's cross-site POST does not carry the `SameSite=Lax` OAuth state cookie. It is cleared once used.

**The ACS.** Looks the entry up by the response's `InResponseTo`, then checks the cookie's binder against it, and consumes the entry only after the binder matches. A request ID leaked in a log therefore cannot burn someone's login. The response is then validated (D-SA-9). A response that fails validation still consumes the entry, and the user starts again. The ACS has its own per-IP bucket (60 a minute, burst 60).

**RelayState** is a fixed opaque value. The return ignores it: the redirect path comes only from the pending entry.

**Replay.** Single use replaces a replay cache: a replayed response finds no pending entry, before or after a restart. A restart drops logins in flight, and the user starts again.

**https.** SAML needs an `https` `public_url` (D-SA-19). The settings API refuses to save or enable a SAML provider otherwise. | A `meta.db` table: a migration and a format bump (spec 04 § Format versions) to keep logins in flight across a restart. A signed state cookie like OIDC's: not sent on the cross-site POST with `Lax`. No browser binding: login CSRF. |
| D-SA-11 | **Metadata: from a URL, refreshed daily; or pasted.**

**From a URL.** The metadata URL is fetched through the guarded function of D-P-53 (https, the egress policy, no redirects, 10 s, 1 MiB). The first fetch happens on save, which answers 400 if the metadata is unusable. After that it is fetched once a day: in grace and during a lapse too, so rotation keeps working, but never in restore mode (`refuse_outbound`).

**Refresh rules.**
- A refresh may change the SSO URL and the certificate set.
- A refresh that changes the certificate set writes an `audit` event and shows "certificates changed on <date>" on the settings page.
- A refresh never changes the IdP entity ID. Metadata with another entity ID counts as a failed refresh.
- A refresh that fails, or that brings unusable metadata, keeps the last good metadata, records the error, and shows it on the settings page.

**Pasted.** Pasted XML, or manual fields (entity ID, SSO URL, certificates), has the same 1 MiB and parse limits, and never refreshes; the owner pastes again.

**What makes metadata usable.**
- The SSO URL is `https`, with no userinfo, and passes the egress policy as a redirect target.
- One to four signing certificates. More than 4 is refused.
- `validUntil` and `cacheDuration` are ignored.

Every signing certificate in it is trusted, so an IdP that publishes its next certificate beside the current one rotates without downtime.

**Certificates are pinned keys.** Their expiry dates are shown, and warned about 30 days ahead on the settings page, but not enforced, because enforcing would lock orgs out at an IdP's expiry.

**Changing the IdP entity ID** is an explicit owner action, a re-import that says the consequence. It deletes the org's identities, like an issuer change (D-P-52).

**Signed metadata is not verified:** trust comes from https or from the owner's paste. | Manual fields only, like Opik (rotation needs the owner on the day); enforcing certificate expiry; verifying metadata signatures (needs a trust anchor the owner would have to paste anyway). |
| D-SA-12 | **Per-org SP endpoints.** The SP entity ID is `<public_url>/api/auth/saml/<org_id>` and serves the SP metadata (no keys; ACS over HTTP-POST; NameID format `unspecified`). The ACS is `…/<org_id>/acs`. They use the org id, not the slug, because a slug can change while the IdP configuration stays. The login entry stays domain-based (D-P-60). An org that is unknown, has no SAML provider, or is dropped answers 404 on both. | One SP entity for the whole instance (Entra wants one app per entity ID per tenant, and the ACS could not know the org before trusting the response); slug paths. |
| D-SA-13 | **Identities.** The identity is `org:<org_id>`, with the NameID's exact text as subject, as OIDC subjects are now. The text is not trimmed, and `NameQualifier` and `SPNameQualifier` are not part of the key. Every NameID format except `transient` is accepted, `emailAddress` included. A later login matches the identity first, then D-P-55's email linking. An IdP that changes its NameID format therefore links to the same account by email, if the email is in a verified domain. The test sign-in, sessions (`sso_org_id`), JIT, enforcement and break-glass are those of org SSO, unchanged. | Keying identities by email (an email change at the IdP would create a second account). |
| D-SA-14 | **SAML lives in `ee`.** `crates/saml` is Elastic-2.0, with its own `LICENSE` and SPDX headers, which `just check-ee` learns. The server and web parts sit under `ee/org_sso` and `web/src/ee/org-sso`. Amends D-P-4's list ("SAML"). | MIT crate: SAML is a paid feature everywhere (§ Competitors). |
| D-SA-15 | **Internal review only (user).** Before the site claims SAML: the hostile corpus in `cetra-saml` (the XSW1–XSW8 wrapping variants, comment injection, DOCTYPE, XXE and entity expansion, duplicate IDs, a second assertion, an unsigned assertion in a signed response, wrong audience, recipient, destination, issuer, `InResponseTo`, times, SHA-1, a foreign certificate in `KeyInfo`, a transient NameID, an encrypted assertion, replay); a fuzz target on response validation (track R M3's setup); an internal security review in the shape of the P26 auth review, with every finding fixed or recorded; and interop by hand against Okta and Keycloak (user). | An external paid review (the user chose internal only); testing Entra and Google Workspace by hand (the user picked Okta and Keycloak). |
| D-SA-16 | **No Single Logout and no other protocol options.** A cetra logout ends the cetra session only, as with OIDC. No Artifact binding, ECP, ForceAuthn, scoping or `RequestedAuthnContext` settings. | SLO: complex (signed Redirect messages both ways) and rarely required for a web console. |
| D-SA-17 | **Storage: one provider row, protocol-specific tables.** `org_sso_providers` keeps `enabled`, `enforced`, `jit_role`, `last_login_at` and `display_name` for both protocols and gains `protocol` (`oidc` or `saml`); its OIDC columns become nullable, with a `CHECK` that ties them to the protocol. The SAML configuration is in `org_sso_saml` (keyed by the provider row, cascade), and its certificates are rows of `org_sso_saml_certificates`, not a JSON column. Domain routing, flags, `set_last_login`, deletion and break-glass therefore work unchanged for both protocols. The migration rebuilds `org_sso_providers`, which is allowed before the first public release (`PRE_RELEASE_NON_ADDITIVE`, with an altering-case test). If a public release ships before M2, the same change becomes a format change with a `META_FORMAT_VERSION` bump. | A SAML table with its own flags (the code survey found eight functions and callers hard-wired to `org_sso_providers`, each of which would have to cover two tables); placeholder values in the OIDC columns for SAML rows. |
| D-SA-18 | **Settings API.** `PUT /api/v1/orgs/{org}/sso/saml` saves a SAML provider: create, replace, or switch from OIDC. `PUT …/sso/provider` stays the OIDC save, and switches back from SAML. `GET`, `PATCH`, `DELETE …/sso/provider` and the domain routes are unchanged. The SAML body takes a display name, the JIT role, exactly one metadata source (a URL, pasted XML, or the entity ID, SSO URL and certificates), optional attribute names, and `confirm_entity_id_change`, which an entity ID change needs (it deletes identities, D-SA-11). The `OrgSso` response gains `saml_sp` (the values to copy into the IdP), and `OrgSsoProvider` gains `protocol` and a `saml` object; its `issuer` and `client_id` become optional (OIDC only). | One PUT with a tagged union, which would change every OIDC client's body. |
| D-SA-19 | **URLs and https.** A SAML provider needs `public_url` to be `https`, with no exception: the binder cookie must be `Secure; SameSite=None`, and tests use an https public URL. A metadata URL follows the OIDC issuer rule (`issuer_problem`): `https`, or `http` on a loopback host, then the egress policy, which refuses loopback unless `allow_private_network_egress` is set. Replaces D-SA-10's debug-build exception. | A debug-build exception for http public URLs (nothing else in the server is debug-only). |

## Open questions for the user

None. The review of 2026-11-23 confirmed the four that writing raised (user):
1. An unsolicited response restarts an SP-initiated login (D-SA-5).
2. Metadata from a URL is refreshed daily (D-SA-11).
3. Certificate expiry is warned about, not enforced (D-SA-11).
4. SAML requires an `https` public URL (D-SA-10).

## Milestones

In order: each depends on the previous one. Q's federation can start after M1.

### M1 — `cetra-saml`

**Delivers:**
- The `crates/saml` crate (Elastic-2.0, no workspace dependency): metadata parsing, AuthnRequest building, response validation to D-SA-9, the solicited check, and the dev-only test IdP.
- The full hostile corpus (D-SA-15) and a fuzz target on response validation.
- `cargo deny` passing on both graphs, with bergshamra's footprint and exact versions recorded.
- The crate dependency graph in `AGENTS.md` (`saml` depends on no workspace crate; `server` depends on it) and the `ee` licence checks extended to it.
- The request to track Q (§ Cloud).

**Depends on:** nothing.

**Leaves out:** any server, storage or UI change.

### M2 — SAML providers on the server

**Delivers:**
- The sign-in core extracted from `org_sso::login::finish` (§ Parts), with the OIDC tests still green.
- An org's provider can be SAML:
  - the additive migration with the SAML table (D-SA-2);
  - the `saml` feature and its lapses;
  - the settings API (create, replace, switch protocol, attribute names);
  - the SP metadata endpoint.
- Metadata import from a URL (with the daily refresh), from pasted XML, or from manual fields.
- The login: authorize, then the AuthnRequest, then the ACS with the pending table and the binding cookie, then the shared sign-in core with test sign-in, linking, JIT, enforcement and sessions.
- The ACS `Origin` exemption and its form extractor.
- The unsolicited-response restart with its loop guard, and the new login error codes.
- `cetra admin org-sso disable` for SAML providers, with a test.
- The spec updates (04, 08, 09, 11, 12), `openapi.json` and the `fixtures/api` examples.
- An end-to-end test: the server against the crate's test IdP, through a real HTTP POST. It covers the checks only the server can make: a binder mismatch, a replay before and after a restart, a leaked request ID without the cookie, the per-IP and per-org bounds, an enforced org, and a test sign-in by a non-owner.

**Depends on:** M1.

**Leaves out:** the web UI and the docs.

### M3 — Settings page and docs

**Delivers:**
- The org SSO page with the protocol choice and the SAML form: metadata URL, XML or manual fields, attribute names.
- The values to copy into the IdP, the trusted certificates with their expiry, the refresh status, and the locked state without `saml`.
- The login error messages.
- The docs: `platform/sso.mdx` (SAML setup, Okta and Keycloak walk-throughs, the tile URL), `plans-and-licence.mdx` and `self-hosting/authentication.mdx`, written as final. The track has one PR, merged only after M4, so nothing is published before the review and interop.
- The `CHANGELOG.md` line.

**Depends on:** M2.

**Leaves out:** marketing-site claims.

### M4 — Review, interop and release

**Delivers:**
- The internal security review (D-SA-15) over `cetra-saml` and the server's SAML paths, with every finding fixed or recorded with a decision.
- Interop by hand against an Okta developer org and Keycloak, following the docs page as written. These are the user's hands-on steps.
- The docs corrected from the interop runs. After the PR merges, the marketing site may add SAML (D197's rule).

**Depends on:** M3.

**Leaves out:** the cloud. Q ships Enterprise SAML on its own schedule, with the crate as reviewed here.
