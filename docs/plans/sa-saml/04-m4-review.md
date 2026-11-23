# SA · M4 — Review, fixes and interop

Milestone 4 of track SA ([`README.md`](README.md)): the internal security review (D-SA-15), its fixes, and interop by hand.

## Reviews held (2026-11-23)

1. **`cetra-saml` security review** (Sonnet, after M1): no critical or high finding. Its medium and low findings were fixed by SA1.9 ([`01-m1-crate.md`](01-m1-crate.md) § SA1.9).
2. **Server security review** (Sonnet, after M2 and M3): no critical or high finding; two medium, seven low. Dispositions below; fixes in SA4.1.
3. **M2/M3 conformance review** (Sonnet): no blocker in the SAML code. One process finding (marketing claims, below) and four minor items, fixed in SA4.1.

### Server review findings

| # | Finding | Disposition |
|---|---|---|
| M1 | The binder cookie has no prefix: a sibling subdomain can toss a `cetra_saml` cookie with `Domain=` and log a victim in as the attacker (login CSRF). The OIDC state cookie has the same weakness. | **Fixed (SA4.1, D-SA-20):** `__Host-` cookies with `Path=/`, and a request carrying two binder cookies is refused. |
| M2 | The ACS and start rate limits key on the raw IPv6 address; rotating within a /64 bypasses them, and each ACS request parses up to 256 KiB. | **Fixed (SA4.1):** both limits key on `network_key(ip)` (/64). |
| L1 | A protocol switch, disable or delete between validation and `admit` still writes the identity. | **Fixed (SA4.1):** `admit` re-checks the provider row id, protocol and `enabled` (unless test) in its transaction. |
| L2 | A confirmed entity-ID change keeps `enabled` and `enforced` on, enforcing against an untested IdP. | **Fixed (SA4.1, D-SA-21):** refused while enforced; a confirmed change turns `enabled` off, so a new test sign-in is needed. |
| L3 | Any cross-site POST of an unsigned unsolicited Response sets the restart cookie (a 2-minute nuisance). | **Accepted:** nothing in it is trusted, the redirect target is fixed, and the effect is one failed IdP-initiated login. |
| L4 | Eviction can be gamed by a distributed attacker (ties, and the global cap across orgs). | **Accepted residual:** needs ~1 000–2 000 distinct /64s; rate limits now key on /64. Revisit if seen. |
| L5 | 404 vs 302 reveals SAML use to someone holding an org UUID; refused ACS posts log at `info`. | **Accepted:** the UUID is random; the /64 rate limit bounds log volume. |
| L6 | Whoever controls the metadata URL's content can rotate certificates silently. | **Accepted, as decided (D-SA-11, user):** https only, guarded egress, audit warn line and the "certificates changed" note. Mailing owners on a change is a follow-up. |
| L7 | No rate limit on the owner's metadata fetch. | **Fixed (SA4.1):** the PUT shares the per-org `sso_dns_checks` limiter (10 a minute). |

### Conformance review findings

| # | Finding | Disposition |
|---|---|---|
| 1 | The pricing pages list SAML before interop (`website/src/marketing/data`). | **Kept, user's call:** the docs-coupled pricing test forces the licence list to match `plans-and-licence.mdx`. They go live only when PR #240 merges, which waits for the interop below. |
| 2 | `patch_org_sso`'s write-time check ignores `protocol`, so a concurrent switch to SAML skips the `saml` and https checks. | **Fixed (SA4.1).** |
| 3 | Missing test `sp_metadata_of_an_oidc_org_is_404`. | **Fixed (SA4.1).** |
| 4 | CSRF test asserts 404 only. | Covered by `the_acs_ignores_a_foreign_origin`; no change. |
| 5 | The enforcement race test is sequential. | Accepted; the in-transaction check is in place. |
| 6 | Docs do not say that licences issued before SAML need reissuing (D-SA-4). | **Fixed (SA4.1).** |

## SA4.1 — Fixes

**Goal:** apply the dispositions marked Fixed.

**Edit:** `crates/server/src/ee/org_sso/{saml.rs,login.rs,settings.rs,store.rs}`, `crates/server/src/auth/limits.rs` (only if needed), `docs/spec/08-auth.md`, `website/src/content/docs/docs/platform/plans-and-licence.mdx`, `website/src/content/docs/docs/platform/sso.mdx`.

**Steps:**
1. **`__Host-` cookies (M1, D-SA-20).** In `saml.rs`:
   - `SAML_COOKIE` becomes the literal `"__Host-cetra_saml"` and `SAML_RESTART_COOKIE` becomes `"__Host-cetra_saml_restart"`. Keep `cetra_core::cetra_name!` if it can build them; otherwise use literals with a comment that the `__Host-` prefix forbids `Domain` and requires `Path=/` and `Secure`.
   - Both cookies use `Path=/` instead of the ACS and SAML paths. `begin`, the unsolicited branch, `fail` and the success path all write `Path=/`.
   - `named_cookie` gains a sibling `fn binder_cookie(headers) -> Result<Option<String>, ()>`, which returns `Err(())` when more than one `__Host-cetra_saml` cookie is present. The ACS treats `Err` as `refuse(SamlInvalid)` before `take`.
   - Update the tests: the authorize test asserts the cookie name `__Host-cetra_saml`, `Path=/`, `Secure`, `HttpOnly`, `SameSite=None`; `test_support::begin_saml` reads the `__Host-cetra_saml=` cookie; new test `two_binder_cookies_are_refused`.
   - Spec 08 § SAML providers: the cookie names, `Path=/`, and that only one SAML login can be in flight per browser (a second one, for any org, replaces the binder).
2. **/64 rate limits (M2).** In `saml.rs`, `start` and the ACS call `check(&crate::auth::limits::network_key(ip), now)` on `sso_discover` and `saml_acs`. Test `the_acs_limit_counts_an_ipv6_slash_64`: 60 POSTs from `2001:db8::1` and the 61st from `2001:db8::2` → 429 (`TestRequest::peer`).
3. **Provider re-check in `admit` (L1).** `SignIn` and `Admit` gain `provider_id: Uuid` and `protocol: SsoProtocol`.
   - Callers pass the values of the provider they validated against. `OrgProvider` does not hold the row id today, so add `pub(crate) id: Uuid` to `OrgProvider`, set in `reload` from `row.id`.
   - At the start of `admit`'s writes (after its reads), `store::provider_tx(conn, org)` must return a row with the same id and protocol, and `enabled` unless `input.test`. Otherwise return `Admitted::SsoUnavailable`.
   - Test `a_provider_switched_during_sign_in_is_unavailable`: call `sign_in_core` with a `SignIn` whose `provider_id` does not match the stored row → `SsoUnavailable`.
4. **Entity-ID change (L2, D-SA-21).** In `put_org_saml_provider`'s write closure:
   - an entity-ID change on an enforced SAML provider is a 409 `"Turn off enforcement before changing the identity provider's entity ID."` (checked on `current`, before the confirmation check);
   - a confirmed change sets `enabled = 0` (in `store::put_saml_provider`, the SAML-to-SAML branch sets `enabled = 0` together with `last_login_at = NULL` when the entity ID differs).

   Tests: `an_entity_id_change_is_refused_while_enforced`, and `a_confirmed_entity_id_change_disables_the_provider`.
5. **PATCH protocol race (conformance 2).** In `patch_org_sso`'s write-time comparison, add `current.protocol != snapshot.protocol` to the conditions that return `Patch::Changed`.
6. **Metadata fetch limit (L7).** In `put_org_saml_provider`, before `fetch_metadata`, `state.limits.sso_dns_checks.check(&org, now)` → `ApiError::RateLimited { retry_after_s }` as the domain verify does. Test `metadata_fetches_are_rate_limited_per_org` (11 URL saves in a row → the 11th is 429).
7. **Missing test (conformance 3).** `sp_metadata_of_an_oidc_org_is_404` in `saml.rs`, with `configure_org_sso`.
8. **Docs (conformance 6).** `plans-and-licence.mdx`, after the table: `Licence keys issued before SAML existed do not list \`saml\`; ask for a new key to use it.` `sso.mdx`, in "Certificates and metadata": a changed entity ID turns the provider off until it is tested again, and is refused while single sign-on is enforced.

**Decisions** (also in `docs/mvp-decisions.md` § Track SA and `README.md`):
- **D-SA-20:** SAML cookies are `__Host-cetra_saml` and `__Host-cetra_saml_restart`, `Path=/`, `Secure; HttpOnly; SameSite=None`. A request with two binder cookies is refused. One SAML login per browser at a time. Rejected: path-scoped cookies, because a sibling subdomain could toss them.
- **D-SA-21:** an IdP entity-ID change is refused while SSO is enforced, and a confirmed change turns the provider off until a new test sign-in, as a protocol switch does. Amends D-SA-11.

**Done when:** `cargo test -p cetra-server --lib ee::org_sso`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo fmt --all --check`, and `just check-docs` pass.

**Commit:** `Apply the SAML security review (D-SA-20, D-SA-21)`

## Interop by hand (the user)

Following `website/src/content/docs/docs/platform/sso.mdx` § Set up SAML, on a server with an https public URL and a licence listing `org_sso` and `saml`:
1. **Okta** (free Integrator org): create the SAML app, import the metadata URL, verify a domain, test sign-in, enable, sign in as a second user (JIT), enforce, then try a password login (refused).
2. **Keycloak** (Docker, behind https): the same steps with a SAML client.
3. Correct the docs where they diverge, then merge PR #240.
