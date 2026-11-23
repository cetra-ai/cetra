# EA · Enterprise admin: audit logs and SCIM — design

Track EA adds the two enterprise administration features that D178 and D-P-4 promise to Enterprise and the self-hosted licence, but that nobody has designed yet:

- an **audit log**: a durable, queryable and exportable record of who did what, per org and for the instance;
- **SCIM 2.0** provisioning: an org's identity provider (Okta, Entra ID) creates, updates and removes the org's members and sets their roles.

The user asked for both on 2026-11-23 to strengthen the self-hosted licence ($5,400 a year) against competitors whose enterprise self-hosted tiers cost $20k–$60k+ a year (D197). SAML is a separate track. Support is D197 and needs no code.

The product questions were answered by the user on 2026-11-23 (§ Settled by the user). Decisions are `D-EA-<n>`, recorded in `docs/mvp-decisions.md` § Track EA.

## Goals

1. Every security-relevant or configuration change, sign-in and data export leaves a durable record, written in the same transaction as the change.
2. Org owners and admins read their org's log in the UI and the API, filter it and export it as CSV or NDJSON. Instance admins do the same with the instance log.
3. Records identify actors and targets by id only. Nothing in a record is an email, a name, a secret or payload content (AGENTS.md, D150). The UI and exports show the current name and email at read time.
4. A signed HTTPS stream sends each org's records to a SIEM.
5. An org's IdP provisions and deprovisions its members over SCIM 2.0 (Users and Groups) and sets their roles through group mappings. Okta and Entra ID work with their stock SCIM apps.
6. Deprovisioning in the IdP removes access in cetra within one request: membership, sessions and personal access tokens.
7. Both features are `ee` entitlement features (`audit_log`, `scim`), granted by Enterprise on the cloud and by the self-hosted licence. Lapses never widen access (D-P-8).

## Non-goals

- **Tamper evidence** (hash chains, signed records, WORM storage). Records are append-only in the API, and nothing edits them. An operator with file access to `meta.db` can still change them. The SIEM stream is the answer for customers who need an off-box copy. Can be revisited later.
- **Recording every read.** Trace, session and page views are not recorded. Only data leaving the product is: SQL console runs, exports and MCP tool calls (D-EA-6).
- **Before/after snapshots of whole objects.** Langfuse stores whole objects unredacted, and LangWatch leaked a private key that way (finding H2). Records carry a typed, allow-listed `details` object per action instead (D-EA-5).
- **SCIM Bulk, sorting, ETags, `/Me`, password sync, custom schemas beyond the enterprise user extension.** None of them are needed by Okta or Entra ID.
- **SCIM as an instance-wide user directory.** SCIM manages one org's members. It never creates instance admins and never touches accounts outside the org's verified domains (D-EA-13).
- **SAML** (its own track), and **custom roles** (D177, later).
- **Syslog, S3 or vendor-specific sinks.** The stream is generic HTTPS. Splunk HEC, Datadog, Elastic and Sumo Logic all accept it through their HTTP intake, or through a small forwarder.

## What competitors do

Full notes: `side-session-notes/…/ea-competitors.md` (Langfuse, LangWatch; Phoenix lists an audit log as a non-goal; Opik, Laminar, Helicone and MLflow have neither feature).

**Audit logs.**
- **Langfuse** records about 45 resource types (create, update, delete) with whole before/after objects, on every plan, so an upgrade shows past history. Its cloud adds IP, user agent, outcome and opt-in read events in ClickHouse, with no TTL. The UI has no filters, export is a batch job, and there is no stream. Gated by `audit-logs` (cloud enterprise or self-hosted licence).
- **LangWatch** logs every tRPC mutation (refused ones too) with args, IP, user agent and before/after, and never deletes rows. It has filters (user, action, project, date) and a CSV export, no stream, and is Enterprise only. Storing raw args leaked a signing key into the log (H2). The fix was field-name redaction plus a purge runbook.

**SCIM.**
- **Langfuse** has Users only, with no Groups. Auth is an org API key over HTTP Basic. Only `userName eq` filters work, and other filters are silently ignored, so they return the whole org. PATCH only accepts lowercase `replace` of `active`, so Entra fails. Roles come from a non-standard `roles` attribute. Deprovisioning removes the membership only, and sessions stay. It learned to ignore a POSTed password (pre-registration of someone else's email), and to check org membership on every global user id.
- **LangWatch** has Users and Groups. Its bearer token is per SSO connection and hashed. Unknown filters get 400. Groups map to role bindings at org, team or project scope, and access is the union of the bindings. Deprovisioning removes the membership, deactivates the account and revokes sessions. It handles the Entra and Okta quirks (capitalised ops, `"False"` strings, three spellings of name patches, member removal by a filter path). SCIM calls themselves are not audited.

What we take: always recording (Langfuse); IP and user agent; filters and export (LangWatch); Groups mapped to roles at org and project scope (LangWatch); hashed per-org tokens; 400 on unsupported filters; the Entra and Okta quirks list; deprovisioning that revokes sessions.

What we avoid: whole-object snapshots and raw args; silently ignored filters; global user ids in SCIM; adding people outside the org's control; leaving SCIM calls out of the audit log.

## Architecture

```
            ┌────────────────────── crates/server ──────────────────────┐
 handler ──►│ change in meta.db ─┐                                      │
            │                    ├─ one transaction ─► audit_events      │
            │ audit::record() ───┘        │                              │
            │      └─► tracing target=audit (stdout, unchanged role)     │
            │                             │                              │
            │  ee::audit::read   ◄────────┤  GET …/audit-events, export   │
            │  ee::audit::stream ◄────────┘  cursor → HTTPS NDJSON → SIEM │
            │                                                           │
 IdP ──────►│  /scim/v2/*  ─► ee::scim ─► scim_users, scim_groups,      │
 (bearer)   │                  │          group mappings                │
            │                  └─► members::*, sessions, PATs (reused)  │
            │                  └─► audit::record() (actor = scim token) │
            └───────────────────────────────────────────────────────────┘
```

### Where the code goes

- **Recording is core (MIT).** `audit::record` replaces today's about 134 inline `tracing::info!(target: "audit", …)` calls, and every org records whatever its plan (D-EA-1). It is D150's "auth events log" made durable, and recording has to run on every plan, or an upgrade would show no history.
- **Everything that reads, exports or streams the log is `ee`.** So is all of SCIM: `crates/server/src/ee/audit/`, `crates/server/src/ee/scim/` and `web/src/ee/` (D-P-1, D-P-2, with SPDX headers checked by `just check-ee`).
- D-P-4 is amended to say "reading, exporting and streaming audit logs".

### Audit log

#### Record format (`meta.db`, table `audit_events`)

| Field | Type | Meaning |
|---|---|---|
| `seq` | `INTEGER PRIMARY KEY AUTOINCREMENT` | The sort, paging and stream cursor key (D-EA-20). `meta.db` has one writer, so `seq` is commit order: a cursor never skips a record that commits later. |
| `id` | UUIDv7 | The record's public id. Not a cursor: UUIDv7 is not ordered within a millisecond, and the clock can step back. |
| `at` | `i64` ns | Write time, UTC. |
| `org_id` | UUIDv7, null | Null: an instance record (D-EA-8). |
| `project_id` | UUIDv7, null | Set when the action is project-scoped. |
| `action` | text | A dotted name from the catalog, e.g. `member.role_changed` (D-EA-4). Append-only names. |
| `outcome` | `success` \| `failure` | `failure` only for actions the catalog marks as recording failures: sign-ins of a known account and licence install. |
| `actor_kind` | text | `user`, `api_key`, `scim_token`, `control_plane`, `cli`, `system`, `anonymous` (D-EA-3). |
| `actor_id` | text, null | User id, key id, SCIM token id, control-plane token id; null for `system`, `cli`, `anonymous`. |
| `via` | text, null | For `user`: `session`, `pat`, `device`; `pat` and `device` carry `credential_id`. |
| `credential_id` | text, null | The PAT id. Never a token value. |
| `target_kind`, `target_id` | text, null | The resource acted on: `user`, `member`, `invite`, `api_key`, `pat`, `project`, `org`, `sso_provider`, `sso_domain`, `scim_token`, `scim_group`, `evaluator`, … A SCIM user operation targets the cetra user id (`user`), with the SCIM row id in `details`, so the record still resolves after the SCIM row is deleted. |
| `details` | JSON | Typed per action, allow-listed fields only (D-EA-5). |
| `ip` | text, null | Client IP by `trusted_proxies` (spec 08 § Client IP). |
| `user_agent` | text, null | Truncated to 256 characters. |

Indexes support the reads: `(org_id, seq)`, `(org_id, action, seq)`, `(org_id, actor_id, seq)` and `(org_id, target_id, seq)`. An instance record has a null `org_id` and is read through the same indexes. `org_id` and `project_id` have no foreign keys, like `deletions`: records outlive the rows they name, and an org drop deletes them explicitly.

#### Writing

- `audit::record(tx, event)` takes the `meta.db` transaction of the change, so a change never commits without its record and a rolled-back change leaves none (D-EA-2).
- **Actions that change nothing in `meta.db`** (sign-ins, SQL runs, exports, MCP tool calls) record in a transaction of their own, **before** the action runs. If that write fails, the action is refused. Such records carry the request's parameters, never a result: no row counts or durations, which are only known after the data has left.
- **Work done later by a worker** (a deletion applied, retention purged, a domain lapsed) records when the worker commits it, with actor `system`.
- **Unauthenticated failures are not stored.** A failed sign-in for an unknown email, or a SCIM call with an unknown token, stays a `tracing` line only, so an anonymous client cannot grow the table. A failed sign-in for a known account is stored, at most one row per account and minute with a count, and an attempt refused by the lockout (`auth/limits.rs`) is not stored. A SCIM call with a revoked or expired token is a tracing line only.
- **An org-provider sign-in writes two rows** in one transaction: the instance record and the org's record (D-EA-9).
- **One request, one record.** A bulk operation (a dataset import, adds by filter, a batch of SCIM group members) writes one record with a count, never one per item.
- **Restore mode** (N's M3) refuses outbound calls and stops workers, not `meta.db` writes. So recording and SCIM run normally, the stream sends nothing, and the hourly audit purge is off, since it cannot be undone on a restored copy.
- A failed audit write fails the request. The alternative, a change with no record, is what an audit log exists to prevent. This is unlike Langfuse, which never lets the audit write fail the caller.
- After commit, the same function emits the `tracing` line with target `audit`, so operators who ship stdout keep their pipeline.
- The event is a Rust enum, one variant per catalog action, each with its typed details. The action name, the target and the `details` JSON come from the variant. Call sites cannot write free-form strings.

#### Event catalog (D-EA-4)

The catalog lists every action, its scope (org or instance) and its `details` fields. Group by group, it covers:

- **Sign-in.** Login (password, instance provider, org provider, device approval), failed login (reason class only; an unknown email has no actor, as `account = "unknown"` today), logout, password change, password reset requested and used, signup, email verified, session revoked.
  - These are instance records.
  - An org-provider login and a JIT join are also recorded in that org's log (D-EA-9).
- **Orgs and projects.** Create, rename, delete requested and applied, ownership transferred; project create, rename, delete requested and applied; retention changed.
- **Members.** Invite created, revoked and accepted; member added (JIT, SCIM), role changed (`details`: old and new role), removed (by whom, or by SCIM); project override set and removed (old and new role).
- **Credentials.** API key created and revoked (kind, scopes, project); PAT created and revoked (scopes, never the value); device code approved.
- **SSO.**
  - Org SSO: provider created, changed (the names of the changed fields, never the secret), enabled, disabled, enforced and unenforced, test sign-in; domain added, verified, lapsed, moved and removed; break-glass disable (actor `cli`).
  - Instance SSO enforcement changes come from configuration and are recorded at startup as `instance.config_applied` with the key names.
- **SCIM.** Token created, rotated and revoked; user provisioned, updated, deactivated, reactivated and deleted; group created, renamed and deleted, members changed; group mapping changed; role raise refused at the seat limit; SCIM enabled and disabled.
- **Audit.** Stream destination created, changed and deleted; export started (format and filters).
- **Data leaving the product (D-EA-6).** SQL console run (a SHA-256 of the SQL text; never the text, which can quote payload values); SQL and table export; dataset export; MCP tool call (tool name, project). Each is recorded before it runs (§ Writing).
- **Deletions.** GDPR deletion requested and applied (kind, counts; the subject's identifier never appears, as the deletion row already keeps it under its own rules).
- **Project objects.** Evaluators, online rules, alert rules and channels, LLM connections, model prices, prompts and protected labels, datasets, annotation queues, issue sources, dashboards and the assistant setting: create, change (field names) and delete. Everything that has an `audit` tracing line today, plus the gaps the code survey found.
- **Instance.** Setup, licence installed and refused (customer and expiry; never the key), admin user created, updated, disabled and reset link issued, org created and dropped by the control plane, control-plane user revoke, disable and delete, entitlements token accepted (plan name, token id).

**Not in the catalog:** work on the data itself rather than on access or configuration: dataset item edits, scores and annotations, queue reviews, playground runs, assistant turns, evaluator executions. Their `target: "audit"` lines become ordinary `info` lines of their module, so `target = "audit"` means exactly the catalog.

The plans for M1 and M2 turn this list into the exact enum, with each variant's details. Every existing `target: "audit"` call site is either replaced by `audit::record` or moved to its module's target; none stays as a bare audit line.

#### Privacy (D-EA-3, D-EA-5)

- Actors and targets are ids. The read API and exports join the current name of a key, project or provider, and the current name and email of a user id.
- In an org's log, the name and email are shown only for users who are still members of the org. Anyone else shows as "Former member `<id>`", so leaving an org stops it seeing your later email. The instance log resolves every existing user.
- A user erased later reads as "Deleted user `<id>`", so an erasure leaves no email in the log.
- `details` fields are allow-listed per action. Secrets, token values, prompts, completions, attribute values, SQL text, email addresses, webhook URLs (only `url_host`, as D-I-18 already does) and names never appear.
- A test walks every variant and refuses a field whose name matches a secret or email pattern, so a new variant cannot add one by accident.
- IP and user agent are personal data. They are kept for the record's retention for security, then deleted with the record. When a user account is deleted, the `ip` and `user_agent` of the records whose actor is that user are cleared in the same transaction.

#### Retention (D-EA-7)

- `audit.retention_days` (default 365, minimum 30, `0` keeps records forever) applies to the instance log and to every org without a limit.
- The org limit `audit_retention_days` (D-P-13), from the org token, replaces it for that org: on the cloud, 365 for Enterprise and more by contract.
- Cloud cells set `audit.retention_days = 90`, so orgs on the other plans keep 90 days, and an upgrade to Enterprise shows the last 90 days.
- An hourly task deletes records older than the retention, in batches.
- An org drop deletes the org's records when the drop is applied (track M's drop path), together with its SCIM tokens, users, groups and mappings. The drop itself is an instance record.
- A user's erasure leaves their records with ids only (above).

#### Read API and UI (ee)

- `GET /api/v1/orgs/{org}/audit-events`. Filters: `from`, `to`, `action` (one or several, or a group prefix like `member.`), `actor_id`, `target_id`, `project_id`, `outcome`. Paging is by cursor on `seq`, newest first, 100 per page by default and 500 at most.
  - Each row carries `actor` and `target` objects with the resolved display fields.
  - Access: new `OrgPerm::ViewAudit` for org admins and owners, with a session or a PAT; no API key (D-EA-10).
- `GET /api/v1/orgs/{org}/audit-events/export?format=csv|ndjson&…` streams the same filters, with no page limit, up to 1,000,000 rows per export, under the same `ViewAudit` permission. It records `audit.export_started` before streaming.
- `GET /api/v1/admin/audit-events` and its export serve the instance log to instance admins. Instance admins never read org logs (no implicit org access, spec 08).
- Without `audit_log`, these routes answer `403 plan_required`. The UI shows the page locked with the offer (D-P-15).
- UI:
  - **Org settings → Audit log**, for admins and owners: a table (time, actor, action, target, project, IP), the filters in the URL, a details drawer, and Export.
  - **`/admin` → Audit log**, the same page for the instance log.

#### SIEM stream (ee, D-EA-11)

- One HTTPS destination per org, set by owners with a session (`OrgPerm::ManageAuditStream`), and one for the instance, set by instance admins, with a URL, an optional header (for example `Authorization: Splunk <token>`, sealed like other secrets) and a signing secret.
- A 10 s task sends each org's new records, oldest first, as an NDJSON batch (at most 500 records or 1 MiB). It signs the body with `X-Cetra-Signature` like alert webhooks, and goes through the egress policy (D166).
- Delivery is at least once. A cursor per destination (the last acknowledged `seq`) moves on a 2xx; failures back off up to 1 h. A destination failing for 24 h emails the owners.
- Retention still applies: records purged before delivery are lost to the stream, and the status page says so.
- The stream payload carries ids only, like the table. A SIEM that wants names joins them on its side, or uses the export.
- Restore mode sends nothing (N's M3).

### SCIM 2.0

#### Endpoints (D-EA-12)

The base URL is one for all orgs: `<public_url>/scim/v2`. The bearer token names the org.

| Endpoint | Supported |
|---|---|
| `GET /ServiceProviderConfig`, `/ResourceTypes`, `/Schemas` | Static documents that advertise exactly what is implemented: `patch: true`, `filter: true` (max 200 results), `bulk`, `sort`, `etag` and `changePassword` false. |
| `/Users` | `GET` (list, `filter`, `startIndex`, `count`), `GET /{id}`, `POST`, `PUT`, `PATCH`, `DELETE`. |
| `/Groups` | The same verbs, with `excludedAttributes=members` honoured (Entra lists groups that way). |

- **Filters.** `userName eq`, `externalId eq` and `emails[type eq "work"].value eq` on Users; `displayName eq` and `externalId eq` on Groups. Anything else is `400 invalidFilter`, never ignored.
- **Errors and statuses.** SCIM error bodies (`urn:ietf:params:scim:api:messages:2.0:Error`, `scimType`). `POST` answers 201 with `Location` and `meta`; `PUT` and `PATCH` answer 200 with the resource; `DELETE` answers 204; an unknown id is a SCIM 404. Content type `application/scim+json`, with `application/json` accepted.
- **Uniqueness.** A `POST /Users` whose `userName` or `externalId` already has a SCIM row in the org answers `409 uniqueness`, and so does a `POST /Groups` with a `displayName` already used in the org. Okta and Entra rely on that answer to match existing resources.
- **Outside OpenAPI.** The `/scim/v2` routes follow RFC 7644, not cetra's API conventions, so their `RouteDef`s are undocumented and stay out of `openapi.json`. The docs page describes them. The Provisioning routes (tokens, enable, mappings) are cetra API and are in OpenAPI. CSRF does not apply: SCIM takes a bearer token only. The route-access suite covers `Access::Scim`.
- **Ids.** SCIM resource ids are the `scim_users` and `scim_groups` row ids (UUIDv7, per org), never cetra's global user ids. A token can only name its own org's rows (the Langfuse pitfall).
- **Users.**
  - Attributes: `userName` (the email; it must be in a verified domain, D-EA-13), `name.givenName`, `name.familyName`, `displayName`, `emails`, `active`, `externalId`, and the enterprise extension's `department` and `employeeNumber`, stored and echoed.
  - `password` is ignored.
  - cetra's `users.name` is set from `displayName`, else from the given and family names, and only while the user belongs to no other org.
- **Groups.** `displayName`, `externalId`, `members` (values are SCIM user ids). Groups exist only to drive role mappings (D-EA-14).
- **Quirks handled.**
  - Ops in any case (`Replace`, `Add`, `Remove`).
  - `active` as `"True"`/`"False"`.
  - PATCH without a path whose value is an object, dotted keys (`name.givenName`), and paths carrying the schema URN (`urn:ietf:params:scim:schemas:extension:enterprise:2.0:User:department`).
  - Group member removal both with a filter path (`members[value eq "…"]`) and with `path: "members"` and an array of values (Entra).
  - Understood ops on unsupported attributes accepted as no-ops.
  - A malformed Groups PATCH never empties a group.
  - Okta's connection test, `GET /Users?startIndex=1&count=1` with no filter.
  - M6 records Okta's and Entra's requests from their validators as fixtures, and tests replay them.

#### Tokens (D-EA-15)

- `scim-cetra-…` bearer tokens per org, at most two active at a time, so a rotation needs no downtime.
- Created, listed (prefix, created by, created and last used) and revoked by owners in Org settings → Provisioning, with a session only (`OrgPerm::ManageProvisioning`, like `ManageSso`, D-P-51).
- Shown once, stored as SHA-256 like API keys.
- A SCIM request resolves to `Principal::Scim { org, token_id }`. Routes declare `Access::Scim`, and every other access kind refuses that principal.
- A per-token rate limit of 600 requests a minute, burst 600 (Entra's initial sync is bursty).
- The `/scim/v2` path joins the API paths in the fallback, so a wrong URL is a SCIM 404, not the web app.
- M5's plan adds `scim-cetra-…` to AGENTS.md § Naming.

#### Who SCIM may provision (D-EA-13)

- SCIM provisions only emails of the org's **verified domains**:
  - self-hosted: D-P-54's `org_sso_domains`;
  - cloud: the domains in the org token (D-EA-17).
- Any other `userName` is `400 invalidValue` ("not in a verified domain of this org"). Outsiders are invited by hand.
- This is the same trust org SSO's JIT already has. It is what lets deprovisioning revoke sessions and PATs: the org controls those identities.
- Enabling SCIM needs at least one verified domain, an owner session, the `scim` feature and, on self-hosted, `org_sso`, since verifying a domain needs it. The licence grants both.
- **Domain changes.** When a verified domain lapses or moves to another org (D-P-54, D-P-72), the members already provisioned under it stay, and SCIM can still update, deactivate and delete them. New creates for that domain are refused. SCIM only ever acts on its own org's `scim_users` rows, so a domain's new holder cannot touch the old holder's members.
- **Create.**
  - An existing account with that email is linked. If its email is not proven (D-P-66), it loses its password, sessions and tokens, exactly as an org-SSO link does.
  - Otherwise a verified user without a password is created, with `email_verified_via = 'idp'`: the IdP asserts an address of a domain the org proved by DNS, the same trust as an org-SSO JIT account. A new value would be a `meta.db` format change (D-R-34, the column's `CHECK`) for no difference in behaviour.
  - Either way the user joins with the role their groups give (D-EA-14), within `max_seats` (`403 plan_limit` as a SCIM error otherwise).
  - Signing in then goes through the org's provider (self-hosted) or the cloud provider, linked by email as today.
  - A password-less user could still set a password through "forgot password", which proves the mailbox. Enabling SCIM does not require SSO enforcement, but the Provisioning page recommends it, and enforcement (D-P-58) closes that path.
- Instance admins can be provisioned as members, but deprovisioning only removes their membership. It never revokes or disables them (D-P-58's break-glass exemption).
- **Email changes** (a new `userName`) apply when the old and new emails are both in the org's verified domains, the new one is free, and the user belongs to no other org. Otherwise `409 mutability`. The account's email is instance-wide, so an org may only rename identities it fully controls.

#### Deprovisioning (D-EA-16)

- **`active: false`.**
  - Removes the org membership and the user's project overrides in the org.
  - Revokes the user's web sessions and personal access tokens, and evicts them from the PAT cache. This is instance-wide, so it also ends their sessions in other orgs, as org SSO enforcement already does (D-P-58). It is allowed because the user's email is in a domain this org proved: the org controls that identity.
  - Disables the account if no membership is left.
  - The SCIM row stays (`active = 0`), with its group memberships.
- **`active: true`** re-adds the membership with the groups' roles, and re-enables the account if SCIM disabled it (`scim_users.disabled_account`).
- **`DELETE`** does what `active: false` does, then deletes the SCIM row and its group memberships. The user account and what it authored (scores, annotations) stay.
- **Owners.** Deactivating or deleting an owner removes their membership, unless they are the org's last active owner: that is refused (`409`, "an org needs an owner"), using the same check as `remove_org_member`. The IdP is the authority once an owner enabled SCIM (user).
- Project API keys are project credentials, not the user's, and stay. The audit records name who created them, so an admin can review them.
- All of this reuses the existing code: the member removal (`members::remove_org_member` with its owner check), the revocation of `routes/internal_users.rs`'s `revoke_access`, and `users::set_disabled`. There is no second implementation.
- `revoke_access` opens its own write today. It is split into a function on a connection, which deletes sessions and revokes PATs and returns the revoked token hashes, and the PAT-cache eviction, which runs after commit. The control-plane route and SCIM both call it. Membership removal, revocation, the account disable and the audit record then commit in one transaction.

#### Roles from groups (D-EA-14)

- An owner maps a SCIM group to an org role (`admin`, `member` or `viewer`) and, optionally, to project roles (`admin`, `member`, `viewer`, `none`) for named projects.
  - The mappings live in `scim_group_mappings`, keyed by group id.
  - The UI lists the pushed groups by `displayName`.
- Roles are ordered `none` < `viewer` < `member` < `admin`.
- A user's org role is the highest org role among their mapped groups, else the org's `scim_default_role` (`viewer` by default, or `member`).
- A user's project role, for each mapped project, is the highest among their groups. A managed member has no override on a project that none of their groups maps.
  - D139 still applies: overrides only exist for org members and viewers, so an org admin's project mappings are ignored.
  - Writing an override needs `project_roles`. Enterprise and the licence have it.
- Mappings never grant or lower `owner`. An owner's org role stays owner while they are provisioned, whatever their groups map to. Only deprovisioning removes an owner (§ Deprovisioning).
- **Seats.** Seats are D176's: owner, admin and member memberships plus pending invites with those roles (D-P-20); viewers are free, so a viewer create never hits the limit. A create as `member` or `admin`, or a raise from `viewer`, that would pass `max_seats` is refused for that user, who keeps their current role (a create gets `403 plan_limit`). A group change that raises several users applies to those within the limit, records `scim.role_raise_refused` for the others, and still succeeds, so one full plan does not block the IdP's whole sync.
- **Materialised.** Every SCIM change recomputes the affected users' roles and writes `org_members` and `project_members` in the same transaction. RBAC (spec 08) is unchanged and reads only those tables.
- **SCIM-managed members.** A member is SCIM-managed while an active `scim_users` row of the org links to them.
  - Members who joined before SCIM was enabled are not managed until the IdP provisions them.
  - When it does, their roles are recomputed from their groups: an admin may be lowered, never an owner. Their manual overrides are replaced by the mapped ones.
- **SCIM only (D-EA-18).** While SCIM is enabled and the org has `scim`:
  - org SSO's JIT join is off for the org;
  - invitations to emails of its verified domains are refused (`409 scim_managed`), and so is accepting one created before SCIM was enabled;
  - for a managed member, these are refused with `409 scim_managed`, and the UI shows "Managed by your identity provider": the org role change (`update_org_member`), the removal (`remove_org_member`, self-leave included), and setting or removing project overrides through the org route and through `ee/project_roles.rs`'s project route.
  - Outsiders are still invited and managed by hand.
- **Lifting the locks.** Disabling SCIM, or a lapse of `scim`, keeps every member and role as it is, turns JIT back on as configured, and makes members editable again. So a lapsed org is never left unable to manage its members.

#### Gating and lapses (D-EA-19)

- `audit_log` and `scim` join D-P-12's append-only feature list, both `ee`, granted by Enterprise (org token) and the self-hosted licence. On a cloud cell the instance log needs nothing: cells are run by the cetra team.
- During a lapse (licence expired, or an org token without the feature), D-P-8 applies:
  - recording continues;
  - reading, export and the stream stop;
  - SCIM still accepts deactivations, deletes, group-member removals and lowering mappings;
  - SCIM refuses creates, reactivations and anything that raises a role (`403` as a SCIM error, `plan_required`).
- So a deprovision in the IdP always reaches cetra, and a lapse never grants access.

#### Cloud (D-EA-17)

- Cloud Enterprise orgs use the same SCIM endpoints on their cell.
- Cells have no verified domains (D183: the control plane federates SSO). So the control plane adds the org's verified SSO domains to the signed org token (`sso_domains`, a new optional claim), and the cell treats them as the org's verified domains for SCIM only (not for routing or enforcement, which stay on the control plane).
- The control plane pushes a new org token whenever the org's domains change (D-P-11). Until then the cell uses the previous list. A domain removed on Q stops new creates once the new token arrives, and already provisioned members stay, as for a lapsed domain.
- Users provisioned on the cell first sign in through the `cloud` provider and are linked by email (D-P-66's `control_plane` proof).
- This is a request to track Q (§ Requests). M5 ships the cell's side. Until Q's token carries `sso_domains`, a cloud org has no verified domains, so enabling SCIM is refused. Cloud SCIM goes live with Q's release.

## Decisions

| ID | Decision | Rejected alternatives |
|---|---|---|
| D-EA-1 | **Always record (user).** Every org and the instance record, whatever the plan. Reading, export and streaming need `audit_log`. Recording is core, reading is `ee`; amends D-P-4's wording. | Record only when entitled: history would start at purchase, and the upgrade sale loses its proof. |
| D-EA-2 | **Same transaction, fail closed.** A record commits with its `meta.db` change. An action that changes nothing in `meta.db` (sign-in, SQL run, export, MCP call) records before it runs. A failed audit write fails the request. The `tracing` line follows the commit. Unauthenticated failures are tracing lines only; a bulk request writes one record. | Best effort after commit (Langfuse): a crash or error leaves a change with no record. Outbox and relay: needless with one SQLite file. |
| D-EA-3 | **Ids only, resolved on read (user).** Actor and target are ids. The API, UI and exports join the current name and email. An erased user reads as "Deleted user". | Snapshot the email (Langfuse Cloud): emails outlive erasure, and AGENTS.md forbids them. |
| D-EA-4 | **A typed catalog.** One enum variant per action, with a dotted append-only name and typed details. It covers access, configuration, deletions and data leaving the product, not edits of the data itself. Every `target: "audit"` call site is replaced, or moved to its module's target. | Free-form strings (both competitors): unfilterable and unreviewable. Logging every mutation generically (LangWatch): it records args, which leak secrets. |
| D-EA-5 | **Allow-listed details, no snapshots.** Changes record field names, plus old and new values only for roles, scopes, flags and limits. A test refuses secret- or email-like field names. | Whole before/after objects: leaked a private key at LangWatch (H2). |
| D-EA-6 | **Data leaving the product is recorded (user).** SQL runs (a hash of the text), exports and MCP tool calls, recorded before they run; no page or trace views. | Writes only: misses exfiltration. Opt-in read events (Langfuse): high volume for little value. |
| D-EA-7 | **1 year, configurable (user).** `audit.retention_days` (default 365, min 30, 0 = forever). The cloud takes the `audit_retention_days` limit from the org token. Cloud cells keep 90 days for orgs without the limit (user). Hourly purge; an org drop deletes the org's records. | Forever (LangWatch, Langfuse ClickHouse): unbounded `meta.db`. 90 days: short for compliance reviews. |
| D-EA-8 | **Org log and instance log (user).** Records with no org form the instance log, read by instance admins on `/admin`. Instance admins never read org logs. | Org logs only: setup, licence and admin actions stay unrecorded. Admins read every org: breaks "no implicit org access". |
| D-EA-9 | **Sign-ins are instance records**, and an org-provider sign-in or JIT join is also recorded in that org's log. | Copying every login to every org of the user: discloses one org's members' activity to another. |
| D-EA-10 | **Owners and admins read (user).** New `OrgPerm::ViewAudit`, with a session or a PAT; no API key. | Owners only: admins run day-to-day membership and need the log. |
| D-EA-11 | **Download first, stream later (user).** M3 has the API, UI and CSV/NDJSON export; M4 adds one signed HTTPS NDJSON destination per org and one for the instance. Delivery is at least once, with a cursor. | Streaming with the page: delays the page. Syslog or S3 sinks: HTTPS covers the major SIEMs. No stream: self-hosted can ship tracing lines, but cloud customers could not. |
| D-EA-12 | **SCIM 2.0 Users and Groups at `/scim/v2`**: the token names the org; per-org resource ids; `eq` filters only, others 400; no Bulk, sort or ETags; the Okta and Entra quirks handled and tested from recorded fixtures. | Users only (Langfuse): Entra assigns by group. A per-org base URL: a second org key in the URL adds nothing to the token. |
| D-EA-13 | **Verified domains only (user).** SCIM provisions only emails of the org's verified domains, links existing accounts with D-P-66's rule, and creates new ones with `email_verified_via = 'idp'` (no format change). Email changes need both emails in the org's domains and no other org membership. | Any email: an IdP admin could add other companies' users without consent, and revoke their sessions. Others as invitations: two lifecycles for one SCIM user. |
| D-EA-14 | **Group mappings (user).** Groups map to an org role and optional project roles. The highest role wins (`none` < `viewer` < `member` < `admin`), with `scim_default_role` for unmapped users. Mappings never grant or lower `owner`. Seat limits refuse single raises without failing the sync. Roles are written to `org_members` and `project_members`. | Org roles only: per-project access is a common enterprise ask. A `roles` attribute (Langfuse): non-standard, and Entra pushes groups. Evaluating groups in RBAC at request time: changes the hot path for no gain. |
| D-EA-15 | **Per-org hashed `scim-cetra-…` tokens**, two active at most, owners with a session only; `Principal::Scim`; a per-token rate limit. | An org API key over Basic (Langfuse): mixes credentials. A token per SSO provider (LangWatch): we have one provider per org. |
| D-EA-16 | **Remove and revoke (user).** Deactivation removes the membership and overrides, revokes sessions and PATs, and disables the account when no membership is left. Reactivation restores both. An owner can be removed, but not the last one (user). Existing functions are reused. | Membership only (Langfuse): a fired employee keeps working sessions. Always disable (LangWatch): locks users out of other orgs. |
| D-EA-17 | **Cloud SCIM on the cell, domains from Q (user).** The org token carries `sso_domains`, used by the cell for SCIM only. A request to track Q. | Self-hosted only at first: Enterprise cloud orgs wait. SCIM on the control plane: much new Q work, and the cell still has to apply it. |
| D-EA-18 | **SCIM only while enabled (user).** A member is managed while an active SCIM row links to them. JIT is off, invitations to verified-domain emails are refused, and manual role changes, override changes and removals of managed members are refused (`scim_managed`). Disabling SCIM or a lapse of `scim` lifts the locks and keeps every role. | Keep both (the IdP overwrites manual changes on the next push): roles flip silently. |
| D-EA-19 | **Lapses never widen access (user, D-P-8).** Recording continues; reads, exports and the stream stop; SCIM accepts removals and lowering changes, and refuses creates, reactivations and raises. Features `audit_log` and `scim` (`ee`; Enterprise, licence). | SCIM fully off when lapsed: IdP deprovisions would not reach cetra. |
| D-EA-20 | **`seq` is the cursor.** `audit_events.seq` (`INTEGER PRIMARY KEY AUTOINCREMENT`, commit order under `meta.db`'s single writer) orders paging, exports and the stream. The UUIDv7 `id` is the public id. | UUIDv7 as cursor: not ordered within a millisecond and exposed to clock steps, so a stream could skip records. `at` as cursor: same problem. |

## Milestones

Recording comes first because it is core and replaces existing code. It is split in two so that SCIM can start after the first half. Then the ee read side, then the stream. SCIM is independent of the audit read side: its operations only call `audit::record`.

### M1 — Audit recording core

- **Delivers:**
  - the `audit_events` table (`seq` cursor) and `audit::record`, in the change's transaction or before the action;
  - the catalog for sign-in, orgs and projects, members, invites and overrides, credentials, SSO and instance events, with those call sites replaced and the gaps filled: logout, signup, session revocation, invite revoked, overrides, org and project create and rename, licence install, and the old and new role on role changes;
  - IP and user agent on every request-driven record;
  - unauthenticated failures kept as tracing lines;
  - the retention key and the hourly purge, the org-drop deletion, and clearing IP and user agent on user deletion;
  - the privacy test over the catalog.
- **Works when done:** every org and the instance write records for access and membership. Nothing reads them yet except tests and `sqlite3`.
- **Depends on:** nothing new.
- **Leaves out:** project objects, data leaving the product, any read API or UI, and feature names (D-P-12: a feature is added by the milestone that first checks it).

### M2 — Audit catalog completion

- **Delivers:**
  - the catalog for project objects, deletions and data leaving the product (SQL runs, exports, MCP tool calls, recorded before they run);
  - the catalog is one enum split into a module per group, so M2 and M5 add variants in different files;
  - one record per bulk request;
  - the remaining `target: "audit"` lines either replaced, or moved to their module's target when they are data edits.
- **Depends on:** M1.
- **Parallel:** may run beside M5. It touches project routes, while M5 touches members, invites and `ee/`.

### M3 — Audit log API, export and pages (ee)

- **Delivers:**
  - the `audit_log` feature (`Feature`, OpenAPI, the web feature names, the licence card);
  - the org and instance read routes with filters and cursor paging;
  - the CSV and NDJSON exports and their `audit.export_started` record;
  - `OrgPerm::ViewAudit`;
  - resolution of names and emails at read time, current members only in org logs;
  - the gating and lapse behaviour;
  - the Audit log pages in org settings and `/admin`, with the locked state;
  - the docs page, the CHANGELOG entry and the website's licence and Enterprise rows (now backed by docs).
- **Depends on:** M2, so the page shows the full catalog.
- **Leaves out:** streaming.

### M4 — SIEM stream (ee)

- **Delivers:**
  - org and instance HTTPS destinations (URL, sealed header, signing secret), `OrgPerm::ManageAuditStream`, and their audit records;
  - the delivery task with cursor, batching, signing, egress policy and backoff;
  - the failing-destination email;
  - restore-mode silence;
  - status on the Audit log page (last delivered, lag, last error);
  - docs for Splunk HEC, Datadog and a generic receiver.
- **Depends on:** M3 (the page and the gating).

### M5 — SCIM Users and tokens (ee)

- **Delivers:**
  - the `scim` feature;
  - SCIM tokens and Org settings → Provisioning (enable, tokens, default role);
  - `/scim/v2` discovery documents and the full Users resource with filters, uniqueness, statuses, PATCH quirks and errors;
  - the verified-domain rule, domain changes and account linking;
  - deprovisioning and reactivation through the shared lifecycle functions;
  - SCIM-managed members, JIT off and the `scim_managed` refusals on every listed route;
  - seats;
  - lapse rules;
  - audit records for every SCIM operation;
  - on the cloud, `sso_domains` read from the org token (live once Q ships it).
- **Works when done:** Okta can provision and deprovision users, all with the default role.
- **Depends on:** M1.
- **Leaves out:** Groups and role mappings.

### M6 — SCIM Groups, role mappings, Okta and Entra (ee)

- **Delivers:**
  - the Groups resource;
  - group mappings to org and project roles, and their UI;
  - recomputation of roles on every change, with the seat rule for group raises;
  - recorded Okta and Entra validator fixtures replayed in tests;
  - docs pages for setting up Okta and Entra ID;
  - the CHANGELOG entry and the website rows.
- **Depends on:** M5.

M3 and M5 both add a feature, routes in OpenAPI and an org settings page. Each plan owns its own entries, and the second to merge regenerates `openapi.json` and `schema.gen.ts`. Migration numbers are assigned when each plan is written, from the code that exists then.

## Requests to other tracks

- **Track Q (control plane):**
  - add an optional `sso_domains` claim to the org token, holding the org's verified SSO domains, for Enterprise orgs with SCIM (D-EA-17);
  - add an `audit_retention_days` limit to the org token for Enterprise (D-EA-7);
  - grant `audit_log` and `scim` to Enterprise.
  - The cell side is in EA's M5 and M1. The fields are optional, so neither side waits on the other's release.
- **Track W (docs site) and the website:** the Audit log and SCIM pages are written by M3, M4, M5 and M6. The pricing rows for the licence and Enterprise flip from "not claimed" to claimed only when those pages exist (D197).

## Spec changes (made by each milestone's plan)

- Spec 04: `audit_events`, the SCIM tables and the token table.
- Spec 08: § Auth events log becomes § Audit log; a new § SCIM; § Authorization gains `ViewAudit`, `ManageProvisioning` and `scim_managed`.
- Spec 09: the routes.
- Spec 11: the pages.
- Spec 12: `audit.retention_days`, the stream, and the features in § Licence keys.

## Open questions for the user

None. The last four were answered on 2026-11-23 (below).

## Settled by the user (2026-11-23)

- Always record; gate reading and export (D-EA-1).
- Ids resolved at read time; no email in records (D-EA-3).
- IP and user agent kept (D-EA-3).
- Download first, stream in a later milestone of this track (D-EA-11).
- Exports, SQL runs and MCP tool calls recorded; no page views (D-EA-6).
- 1 year by default, configurable (D-EA-7).
- Owners and admins read the org log (D-EA-10).
- An instance log for instance admins, beside org logs (D-EA-8).
- SCIM for verified-domain emails only (D-EA-13).
- Deprovisioning removes the membership, revokes sessions and PATs, and disables the account when it has no other membership (D-EA-16).
- Group mappings to org and project roles; never owner (D-EA-14).
- SCIM only while enabled: no JIT, no invitations or manual role changes for verified-domain members (D-EA-18).
- Cloud SCIM on the cell with domains in the org token (D-EA-17).
- Lapses: removals keep working, creates and raises are refused (D-EA-19).
- SQL runs are recorded as a SHA-256 of the text, never the text (D-EA-6).
- A provisioned user with no mapped group gets `scim_default_role`, `viewer` by default (D-EA-14).
- SCIM may remove an owner who is not the last one (D-EA-16).
- Cloud cells keep 90 days of audit records for non-Enterprise orgs (D-EA-7).
