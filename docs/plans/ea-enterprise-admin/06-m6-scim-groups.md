# EA M6 — SCIM Groups, role mappings, Okta and Entra

**Summary.**
- This milestone adds the SCIM Groups resource and the role mappings that turn groups into org and project roles (D-EA-14).
- Every change that can affect a managed member's roles recomputes them in the same transaction: group membership, group deletion, mapping change, default-role change, linking and reactivation. Raises past the seat limit are refused per user with `scim.role_raise_refused`.
- It adds request fixtures for Okta and Entra ID, replayed in tests, a Role mappings card on the Provisioning page, and the Okta and Entra ID setup guides.

**Prerequisites.**
- M5 is committed and green.
- Branch `claude/ea-enterprise-admin` in `/home/sebastien/travaux/llm-observer/.claude/worktrees/ea-design`.
- The next free migration number is `0033`: check it. M4 takes the one after.

**Read first.**
- `docs/plans/ea-enterprise-admin/README.md` § SCIM (Endpoints, Roles from groups, Gating and lapses), D-EA-12, D-EA-14, D-EA-18, D-EA-19.
- `docs/plans/ea-enterprise-admin/05-m5-scim-users.md` (its P-1 to P-8 still apply; P-5 is replaced here).
- The M1 plan's shared rules.
- The facts file `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/ea-m6-code-facts.md` (§0–§8: every function, helper and test helper named below).

## What it is, and what it is not

**What works when it's done:**
- **Groups resource.** The IdP creates, lists, filters (`displayName eq`, `externalId eq`), reads, renames, patches the members of, replaces and deletes groups at `/scim/v2/Groups`. Members are the org's SCIM user ids.
- **Mappings.** An owner maps each group to an org role (admin, member or viewer) and to project roles on Org settings → Provisioning.
- **Roles.**
  - A managed member's org role is the highest of their groups' mapped roles, else the default role.
  - Their project roles are the highest per project, with no override on unmapped projects.
  - Owners are never changed. Org admins get no overrides (D139).
- **Seats.** Raises past `max_seats` are refused for that user, with a record; the request still succeeds.
- **Lapses.** A lapsed org accepts member removals, group deletions and lowering mapping changes, and skips raises.
- **Fixtures.** Requests shaped as Okta and Entra ID send them are replayed in tests.

**Out of scope:**
- **Captured validator runs.** The fixtures are written from the vendors' documented request shapes, because their validators cannot run from here. The README in `fixtures/scim/` says so, and capturing real runs is a follow-up.
- **The website's pricing rows.** The marketing site waits until the product is frozen.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-EA-12 | Groups at `/scim/v2/Groups`; `eq` filters on `displayName` and `externalId`; the Okta and Entra quirks; a malformed Groups PATCH never empties a group |
| D-EA-14 | Mappings to org and project roles; highest wins (`none` < `viewer` < `member` < `admin`); default role for unmapped; never owner; seat rule; materialised in `org_members` and `project_members` |
| D-EA-18 | Managed members' roles and overrides come from SCIM; manual changes refused |
| D-EA-19 | Lapse: removals and lowerings accepted, raises refused |
| D139 | Overrides only for org members and viewers |

**Plan-level choices:**
- **P-1. Migration.** A new `0033_scim_groups.sql` (append-only), not an edit of `0032`.
- **P-2. Recompute everywhere.** `ee::scim::recompute::recompute_tx` is the only code that sets a managed member's org role or overrides from SCIM. It replaces M5's P-5 ("SCIM never changes an existing member's role"): a linked existing member is recomputed at once.
- **P-3. Default-role changes recompute.** Changing `default_role` on the settings route recomputes every active managed member of the org, in the same transaction.
- **P-4. Audit volume.** One request writes at most:
  - one record for the group operation (`scim.group_created`, `scim.group_updated`, `scim.group_deleted`, `scim.group_members_changed` with counts);
  - one `scim.roles_recomputed { users, changed }` when roles changed;
  - one `scim.role_raise_refused { count, scim_user_ids }` when raises were refused, with at most 20 ids (the README's one-record-per-bulk-request rule).

  Group names are not recorded (ids only).
- **P-5. Lapses inside a recompute.** A change is a raise or a lowering by **effective access**, before and after: the effective project role (`auth::roles::effective_project_role(org_role, override)`, `None` meaning no access), ranked none < viewer < member < admin. So removing a `none` override is a raise, and adding a `none` override is a lowering.
  - Without `scim`, a recompute applies lowerings and skips raises, recording no refusal.
  - Without `project_roles`, it applies only override changes that lower effective access.
  - After a lapse and a renewal, roles stay as they were until the next change triggers a recompute: accepted.
- **P-6. Recompute only while SCIM is on.** The settings and mapping routes recompute only when `org_scim.enabled` is true after their write. When SCIM is off, members are edited by hand (D-EA-18, "Lifting the locks").
- **P-7. Org scoping.** Every group route, and every mapping route, first loads the group with `group_tx(conn, org, id)` inside its write (404 otherwise). Store functions taking only a group id are then always called with a group of the path's org.
- **P-8. Order and cost.** A recompute processes users ordered by `scim_users.created_at, id`, so seat allocation is deterministic. It loads the org's mappings once (`mappings_tx`) and the user-to-groups map in one query (`groups_by_user_tx`), not per user.
- **P-9. Shared helpers.** G2 owns `users.rs` and makes its helpers `pub(crate)`: `admit`, `route`, `Scope`, `scope_of`, `ListQuery`, `parse_id`, `json_object`, `base_url`, `write_error` and `evict`. G3 uses them; it never copies them. Write closures keep M5's convention: a refusal found before the first write returns `Ok(Err(ScimError))`, so nothing half-applies.

## Versions

No new dependency.

## Phase graph

```
G1a (tables, store, audit actions, module lines) ──┬── G2 (recompute + hooks)
G1b (filter, patch, schema, errors) ───────────────┼── G3 (Groups endpoints)
                                                   └── G4 (mapping API, OpenAPI, mocks)
G3 ── G5 (fixtures and replay tests)
G4 (+ coordinator's just openapi) ── G6 (web)
G7 (docs) after G2–G4
```

- G1a and G1b run in parallel. G2, G3 and G4 run in parallel after both: their files are disjoint, and they depend on each other only at compile time. The checks run once, at the end.
- G1a's `mod.rs` names modules that later phases create; the tree compiles once G2–G5 are committed.
- Prerequisite: M5's fix commits are in (they are: the tree is clean).

**Shared rules:** M1's shared rules, M5's P-8, the SPDX header on `ee/` files. No group display name, email, name or external id in logs or records.

---

### G1a — Tables, store, audit actions and module lines

**Edit:** `crates/server/migrations/meta/0033_scim_groups.sql` (new), `crates/server/src/ee/scim/store.rs`, `crates/server/src/ee/scim/mod.rs`, `crates/server/src/ee/scim/test_helpers.rs` (new, step 5), `crates/server/src/audit/catalog/scim.rs`, `crates/server/src/audit/mod.rs` (`TargetKind::ScimGroup`), `crates/server/src/audit/tests.rs`.

**Steps:**

1. Migration:

   ```sql
   -- SCIM groups and role mappings (track EA M6, D-EA-14). No CHECK on text columns (D-R-34).
   CREATE TABLE scim_groups (
       id TEXT PRIMARY KEY,
       org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
       display_name TEXT NOT NULL,
       external_id TEXT,
       created_at INTEGER NOT NULL,
       updated_at INTEGER NOT NULL
   );
   CREATE UNIQUE INDEX scim_groups_org_display_name ON scim_groups (org_id, display_name);
   CREATE UNIQUE INDEX scim_groups_org_external_id ON scim_groups (org_id, external_id) WHERE external_id IS NOT NULL;
   CREATE TABLE scim_group_members (
       group_id TEXT NOT NULL REFERENCES scim_groups(id) ON DELETE CASCADE,
       scim_user_id TEXT NOT NULL REFERENCES scim_users(id) ON DELETE CASCADE,
       PRIMARY KEY (group_id, scim_user_id)
   );
   CREATE INDEX scim_group_members_user ON scim_group_members (scim_user_id);
   CREATE TABLE scim_group_mappings (
       group_id TEXT PRIMARY KEY REFERENCES scim_groups(id) ON DELETE CASCADE,
       org_role TEXT,
       updated_at INTEGER NOT NULL
   );
   CREATE TABLE scim_group_project_roles (
       group_id TEXT NOT NULL REFERENCES scim_groups(id) ON DELETE CASCADE,
       project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
       role TEXT NOT NULL,
       PRIMARY KEY (group_id, project_id)
   );
   ```

   - `org_role` is `admin`, `member`, `viewer` or NULL (no org mapping).
   - `role` is `admin`, `member`, `viewer` or `none`.
   - A group with neither an org role nor project rows has no mapping row.
2. **`store.rs` additions**, in the style of the user functions:

   ```rust
   pub(crate) struct ScimGroupRow { pub(crate) id: Uuid, pub(crate) display_name: String, pub(crate) external_id: Option<String>, pub(crate) created_at: i64, pub(crate) updated_at: i64 } // times feed the resource's meta
   pub(crate) enum GroupFilter { DisplayName(String), ExternalId(String) }
   pub(crate) struct GroupMapping { pub(crate) org_role: Option<OrgRole>, pub(crate) projects: Vec<(ProjectId, ProjectOverride)> }

   pub(crate) async fn insert_group(conn, org, display_name: &str, external_id: Option<&str>, now) -> Result<ScimGroupRow, MetaError>; // Conflict("uniqueness")
   pub(crate) async fn group_tx(conn, org, id) -> Result<Option<ScimGroupRow>, MetaError>;
   pub(crate) async fn group(pool, org, id) -> Result<Option<ScimGroupRow>, MetaError>;
   pub(crate) async fn list_groups(pool, org, filter: Option<&GroupFilter>, start_index: u32, count: u32) -> Result<(u64, Vec<ScimGroupRow>), MetaError>;
   pub(crate) async fn all_groups(pool, org) -> Result<Vec<ScimGroupRow>, MetaError>; // ORDER BY display_name, for the settings page
   pub(crate) async fn update_group(conn, org, id, display_name: &str, external_id: Option<&str>, now) -> Result<(), MetaError>; // Conflict("uniqueness")
   pub(crate) async fn delete_group(conn, org, id) -> Result<bool, MetaError>;
   pub(crate) async fn group_member_ids_tx(conn, group: Uuid) -> Result<Vec<Uuid>, MetaError>;
   pub(crate) async fn group_member_ids(pool, group: Uuid) -> Result<Vec<Uuid>, MetaError>;
   pub(crate) async fn member_counts(pool, org) -> Result<HashMap<Uuid, u32>, MetaError>;
   /// Adds the given SCIM users of `org` (unknown or other-org ids are ignored); returns the ids added.
   pub(crate) async fn add_group_members_tx(conn, org, group: Uuid, users: &[Uuid]) -> Result<Vec<Uuid>, MetaError>;
   pub(crate) async fn remove_group_members_tx(conn, group: Uuid, users: &[Uuid]) -> Result<Vec<Uuid>, MetaError>; // returns the ids removed
   pub(crate) async fn groups_of_user_tx(conn, scim_user: Uuid) -> Result<Vec<Uuid>, MetaError>;
   pub(crate) async fn mapping_tx(conn, group: Uuid) -> Result<Option<GroupMapping>, MetaError>;
   pub(crate) async fn mappings(pool, org) -> Result<HashMap<Uuid, GroupMapping>, MetaError>;
   pub(crate) async fn put_mapping(conn, group: Uuid, mapping: &GroupMapping, now) -> Result<(), MetaError>; // replaces the org row and the project rows
   pub(crate) async fn delete_mapping(conn, group: Uuid) -> Result<bool, MetaError>;
   pub(crate) async fn active_user_ids_tx(conn, org) -> Result<Vec<Uuid>, MetaError>; // active scim_users ids, ORDER BY created_at, id
   pub(crate) async fn mappings_tx(conn, org) -> Result<HashMap<Uuid, GroupMapping>, MetaError>;
   pub(crate) async fn groups_by_user_tx(conn, org) -> Result<HashMap<Uuid, Vec<Uuid>>, MetaError>; // scim user id -> its group ids, one query
   ```

   - `add_group_members_tx` inserts with `INSERT OR IGNORE … SELECT ? , id FROM scim_users WHERE org_id = ? AND id = ?`, so another org's ids can never join.
   - Role text is parsed with `roles::parse_org_role` and `parse_project_override`. A bad stored value is `MetaError::Corrupt`.
   - `put_mapping` refuses duplicate project ids before writing (`Conflict("duplicate project")`): the handler validates first, so this is a guard.
   - Display-name lookups use SQLite's `=`, which is case-sensitive.
3. **`mod.rs`:** add `pub(crate) mod groups;`, `pub(crate) mod mappings;`, `pub(crate) mod recompute;`, `#[cfg(test)] mod replay_tests;` and `#[cfg(test)] pub(crate) mod test_helpers;`. `routes()` also extends `groups::routes()` and `mappings::routes()`. G2–G5 create those files, except `test_helpers.rs` (step 5).
4. **Audit:** `ScimAction` gains the variants below (`TargetKind::ScimGroup` is `scim_group`), with samples and the target-kind assert in `audit/tests.rs`.

   | Variant | Name | Target |
   |---|---|---|
   | `GroupCreated { scim_group_id: Uuid, members: u32 }` | `scim.group_created` | `scim_group` |
   | `GroupUpdated { scim_group_id: Uuid, fields: Vec<&'static str> }` | `scim.group_updated` | `scim_group`; `displayName`, `externalId` |
   | `GroupDeleted { scim_group_id: Uuid }` | `scim.group_deleted` | `scim_group` |
   | `GroupMembersChanged { scim_group_id: Uuid, added: u32, removed: u32 }` | `scim.group_members_changed` | `scim_group` |
   | `GroupMappingChanged { scim_group_id: Uuid, org_role: Option<&'static str>, projects: u32 }` | `scim.group_mapping_changed` | `scim_group` |
   | `RolesRecomputed { users: u32, changed: u32 }` | `scim.roles_recomputed` | — |
   | `RoleRaiseRefused { count: u32, scim_user_ids: Vec<Uuid> }` | `scim.role_raise_refused` | — (at most 20 ids) |

5. **`test_helpers.rs`** (`#[cfg(test)]`, SPDX), the shared setup for G2–G5's tests:
   - `pub(crate) async fn scim_org(app: &TestApp, seeded: &Seeded, domain: &str, default_role: OrgRole) -> String` (licensed app expected; writes `put_org_scim(enabled)`, a verified `insert_domain`, `insert_token`; returns the bearer token);
   - `pub(crate) fn bearer(token: &str) -> String`;
   - `pub(crate) async fn create_user(app, token, user_name) -> TestResponse` (POST /scim/v2/Users with the PatchOp-free User body);
   - `pub(crate) async fn create_group(app, token, display_name, members: &[&str]) -> TestResponse`;
   - `pub(crate) async fn scim_request(app, token, method, path, body: &serde_json::Value) -> TestResponse` (sets `content-type: application/scim+json`).

   Copy the bodies from the private helpers in `users.rs`'s tests module (`scim_org_in`, `bearer`, `create`); G2 then deletes those private copies and uses these.

**Tests:**
- store: `group_names_are_unique_per_org`, `members_of_another_org_never_join`, `put_mapping_replaces_project_rows`, `deleting_a_group_cascades_members_and_mappings`, `deleting_a_project_drops_its_mapping_rows`, `groups_of_user_lists_memberships`.

**Commit:** `Add SCIM groups and role mapping tables`

---

### G1b — Group filters, PATCH and resource shapes

**Edit:** `crates/server/src/ee/scim/filter.rs`, `crates/server/src/ee/scim/patch.rs`, `crates/server/src/ee/scim/schema.rs`, `crates/server/src/ee/scim/error.rs`.

**Steps:**

1. **`filter.rs`:**
   - `pub(crate) fn parse_group_filter(input: &str) -> Result<GroupFilter, ScimError>`, on the same grammar: attribute `displayName` or `externalId` (case-insensitive), `eq`, a quoted value of at most 320 characters. Anything else is `ScimError::invalid_group_filter()`.
   - `GroupFilter` is in `store.rs` (G1a).
   - Make `parse_comp_value` `pub(crate)` for `patch.rs`.
2. **`error.rs`:** `invalid_group_filter()` (400 `invalidFilter`, "Only eq filters on displayName and externalId are supported.") and `group_uniqueness()` (409 `uniqueness`, "A group with this displayName or externalId already exists."). The `From<MetaError>` mapping stays user-worded: group handlers map `Conflict("uniqueness")` themselves.
3. **`patch.rs`:**
   - Factor the body checks out of `user_changes` into `fn operations(body: &Value) -> Result<&Vec<Value>, ScimError>`: the schema contains `PATCH_SCHEMA`, `Operations` (or `operations`) is a non-empty array, and there are at most 100 operations. `user_changes` uses it; its behaviour is unchanged.
   - Add:

     ```rust
     #[derive(Debug, Clone, PartialEq)]
     pub(crate) enum GroupChange { DisplayName(String), ExternalId(Option<String>), AddMembers(Vec<String>), RemoveMembers(Vec<String>), ReplaceMembers(Vec<String>) }
     pub(crate) fn group_changes(body: &Value) -> Result<Vec<GroupChange>, ScimError>;
     ```

   - Member values stay strings; the handler parses UUIDs and resolves them against the org. Ops are matched case-insensitively.
   - **`add`:**
     - with path `members` and an array of `{value}` objects → `AddMembers`;
     - without a path and with an object value: `members` → `AddMembers`, `displayName` → `DisplayName`, `externalId` → `ExternalId(Some)`.
   - **`replace`:**
     - path `members` + array → `ReplaceMembers`;
     - path `displayName` + string → `DisplayName`;
     - path `externalId` → `ExternalId`;
     - no path + object (Okta's `{"op":"replace","value":{"id":…,"displayName":…}}`): `displayName` → `DisplayName`, `members` → `ReplaceMembers`, with `id` ignored.
   - **`remove`:**
     - path `members[value eq "<id>"]` → `RemoveMembers([id])`: match `members[` case-insensitively, require the path to end with `]`, strip it and trim, then match `value`, spaces, `eq` and spaces, and pass the rest to `filter::parse_comp_value`. Its error maps to `invalid_value("Unsupported members path.")`, not the user-worded filter error;
     - path `members` with a value array of `{value}` (Entra) → `RemoveMembers`;
     - path `members` without a value or with an empty array → `invalid_value("Removing every member needs a replace with an empty list.")`: a malformed PATCH never empties a group;
     - path `externalId` → `ExternalId(None)`.
   - Unknown paths are no-ops (D-EA-12).
   - A `displayName` value that is not a non-empty string is `invalid_value`.
   - Member entries without a string `value` are `invalid_value`.
   - `ReplaceMembers(vec![])` (an explicit empty array on `replace`) is allowed: it is how an IdP empties a group on purpose.
   - A `replace` or `add` of `members` whose value is not an array (null, a string) is `invalid_value`.
4. **`schema.rs`:**
   - `pub(crate) const GROUP_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";`
   - `pub(crate) fn group_resource(row: &ScimGroupRow, members: Option<&[Uuid]>, base_url: &str) -> Value`. It holds `schemas`, `id`, `displayName`, `externalId` when set, and `members: [{"value": id, "$ref": "<base>/Users/<id>"}]` when `members` is `Some`. Without them (`excludedAttributes=members`) the key is omitted. `meta` has `resourceType: Group`, `created`, `lastModified` (as `user_resource` formats them) and `location: <base>/Groups/<id>`. No member `display`.
   - `resource_types` lists User and Group (count 2); `schemas` adds the core Group schema with `displayName` (required), `externalId` and `members` (multi-valued complex: `value`, `$ref`), count 3. Update the count assertions in the tests.

**Tests:**
- filter: `parses_display_name_eq`, `refuses_member_filters_on_groups`.
- patch:
  - `okta_add_members_with_path`, `okta_replace_display_name_without_path`;
  - `entra_remove_members_by_value_array`, `entra_remove_member_by_filter_path`;
  - `remove_members_without_value_is_invalid`, `replace_members_with_empty_list_empties_on_purpose`;
  - `capitalised_ops_on_groups`, `unknown_group_path_is_a_noop`;
  - `user_patch_still_validates_the_body` (regression for the factoring).
- schema: `group_resource_omits_members_when_excluded`, `discovery_lists_users_and_groups`.

**Commit:** `Add SCIM group filters, PATCH and resource shapes`

---

### G2 — Recompute and its hooks

**Edit:** `crates/server/src/ee/scim/recompute.rs` (new), `crates/server/src/ee/scim/users.rs`, `crates/server/src/ee/scim/settings.rs`, `crates/server/src/meta/members.rs` (one new function, step 1).

**Steps:**

1. **`recompute.rs`:**

   ```rust
   /// What the caller knows outside the write (entitlements are in memory).
   #[derive(Debug, Clone, Copy)]
   pub(crate) struct RecomputeScope { pub(crate) org: OrgId, pub(crate) now: i64, pub(crate) default_role: OrgRole, pub(crate) max_seats: Option<u64>, pub(crate) scim: bool, pub(crate) project_roles: bool }
   impl RecomputeScope { pub(crate) fn of(state: &AppState, org: OrgId, default_role: OrgRole) -> Self; } // max_seats from resolve(org, now); scim and project_roles from entitlements::of(..).allows(..)

   /// D-EA-14: recomputes the org role and project overrides of each listed SCIM user of the org
   /// from its groups' mappings, in the caller's transaction, and writes the records of P-4.
   pub(crate) async fn recompute_tx(conn: &mut SqliteConnection, ctx: &audit::Ctx, scope: &RecomputeScope, scim_users: &[Uuid]) -> Result<(), MetaError>;

   /// The org role a SCIM user's groups give (highest mapped, else `default_role`), from preloaded maps.
   pub(crate) fn target_org_role(groups: &[Uuid], mappings: &HashMap<Uuid, GroupMapping>, default_role: OrgRole) -> OrgRole;

   fn effective_rank(role: Option<Role>) -> u8; // None 0, Viewer 1, Member 2, Admin 3 (Role = the effective project role type of auth::roles)
   ```

   Use `auth::roles::org_rank` for org roles; do not redefine it.

   In `meta/members.rs`, add `pub(crate) async fn project_overrides_of_user_tx(conn, org: OrgId, user: Uuid) -> Result<Vec<(ProjectId, ProjectOverride)>, MetaError>` (`SELECT pm.project_id, pm.role FROM project_members pm JOIN projects p ON p.id = pm.project_id WHERE p.org_id = ? AND pm.user_id = ?`, parsed with `parse_project_override`).

   ```rust
   ```

   Load `mappings_tx` and `groups_by_user_tx` once. Then **per user**, deduplicated and ordered by `scim_users.created_at, id` (P-8):
   1. Load the row (`user_tx`). Skip a missing or inactive row.
   2. `current = members::org_role_tx(conn, org, user_id)`. Skip a non-member (an inactive state the deactivation already handled) and an `Owner`. Owners are never changed, and D139 keeps them project admins.
   3. The **target org role** is `target_org_role(groups of the user, mappings, scope.default_role)`.
   4. **Raise check** (`org_rank(target) > org_rank(current)`):
      - when `!scope.scim`, keep `current` (P-5);
      - when `current == Viewer`, run the seat check of `users::ensure_member_tx` (`seat_members_tx + seat_invites_tx(…, Some(email))`, `over_limit(max_seats, seats, 1)`), with the account email from `users::user_by_id_tx`. When over, add the user to the refused list and keep `current` for the org role. Project roles are still computed with the role kept.
      - after a raise is applied, delete the org's pending invite for that email, as `ensure_member_tx` does.
   5. Set the org role when it changed (`members::set_org_role`), and count it in `changed`.
   6. **Overrides**, for the final org role. Read the existing ones with `project_overrides_of_user_tx`.
      - **`Admin`:** remove them all (D139: admins have none). Count each removal.
      - **Otherwise:** the desired map is project → the highest override among the groups' project rows (none < viewer < member < admin). For each project in the union of the existing and the desired:
        - compare effective access before and after (P-5);
        - when they differ, apply the change (`set_project_override` or `remove_project_override`) if it lowers access, or if `scope.scim && scope.project_roles`; otherwise skip it;
        - count each applied change.
   7. After all users:
      - when anything changed, record `RolesRecomputed { users: <users processed>, changed }` with `.org(org)`;
      - when raises were refused, record one `RoleRaiseRefused { count, scim_user_ids: <first 20> }` with `.org(org)`.

   `ctx` is the caller's (a SCIM token, or the owner on the settings routes).
2. **`users.rs`:**
   - Make the helpers of P-9 `pub(crate)`, and move the tests onto `ee::scim::test_helpers`, deleting the private copies of those helpers.
   - **Create:** when the user was already a member (`ensure_member_tx` returned `None`), call `recompute_tx(…, &[scim_user_id])` after `insert_user`. This replaces M5's P-5; remove that comment.
   - **Reactivation**, in this order:
     1. `set_user_active(true)`;
     2. compute `target_org_role` from the user's groups (load `mappings_tx` and the user's groups);
     3. `ensure_member_tx` with that role, so a viewer-mapped user is not added at a higher default and does not hit the seat limit;
     4. `recompute_tx(…, &[id])` for the overrides.
   - Use a `RecomputeScope` built before the write from `scope_of`'s values plus `project_roles`.
3. **`settings.rs`:** when `default_role` changes and SCIM is enabled after the write (P-6), call `recompute_tx` over `store::active_user_ids_tx(conn, org)` after `put_org_scim`, in the same transaction (P-3), with the owner's ctx. Build the scope with the **new** default role.

**Tests** (`recompute.rs`, with `ee::scim::test_helpers`):
- `highest_mapped_role_wins`, `unmapped_users_get_the_default_role`, `owners_are_never_changed`;
- `admins_get_no_overrides`, `unmapped_projects_lose_their_overrides`;
- `raise_past_the_seat_limit_is_refused_and_recorded`, `member_to_admin_needs_no_seat`;
- `lapsed_org_lowers_but_never_raises`, `without_project_roles_no_new_override_is_written`;
- `changing_the_default_role_recomputes_unmapped_members`, `a_default_role_change_with_scim_off_recomputes_nothing`, `linking_an_existing_member_recomputes_at_once`;
- `removing_a_none_override_is_a_raise_and_is_skipped_when_lapsed`;
- `reactivation_uses_the_groups_role`;
- `one_roles_recomputed_record_per_request`, `refused_raises_write_one_record_with_a_count`.

**Commit:** `Recompute SCIM members' roles from their groups`

---

### G3 — The Groups resource

**Edit:** `crates/server/src/ee/scim/groups.rs` (new).

**Steps:**

1. **`pub(crate) fn routes()`**, with `Access::Scim` and `documented: false`, built with `users::route` (made `pub(crate)` by G2, P-9):
   - `GET /scim/v2/Groups`, `POST /scim/v2/Groups`;
   - `GET`, `PUT`, `PATCH` and `DELETE /scim/v2/Groups/{id}`.

   Each handler starts with `users::admit`, and uses `users::{scope_of, parse_id, json_object, base_url, write_error, ListQuery}` (P-9). Every handler on `/Groups/{id}` loads the group with `group_tx(conn, org, id)` first (P-7).
2. **`GET /Groups`:**
   - `filter` (`parse_group_filter`), `startIndex` and `count` with the users rules;
   - `excludedAttributes=members` (case-insensitive, comma list) omits members;
   - answers `list_response` of `group_resource`.
3. **`POST`** (201 + `Location`):
   - requires `scim` (else `forbidden`);
   - `displayName` is required (`invalid_value`); `externalId` is optional; `members` is optional (`[{value}]`);
   - in one write: `insert_group` (`Conflict("uniqueness")` → `group_uniqueness()`), `add_group_members_tx`, `recompute_tx(added)`, and record `GroupCreated { members }`.
4. **`GET /{id}`:** the resource with members, or 404.
5. **`PUT`** (200 with the full resource, including members): replace `displayName`, `externalId` and members (an absent `members` keeps the current ones; the RFC lets clients omit it). Compute added and removed, recompute both sets, and record `GroupUpdated` when the name or external id changed and `GroupMembersChanged` when the members did.
6. **`PATCH`** (200 with the full resource, including members):
   - apply `group_changes` in order;
   - members are parsed as UUIDs and resolved to the org's SCIM users. Unparseable or unknown values are ignored, except when a non-empty `ReplaceMembers` (or a PUT `members`) resolves to no known user: that is `invalid_value("No listed member is a user of this organization.")`, so a malformed request never empties a group;
   - recompute the union of added and removed, and record as for PUT.
7. **`DELETE`** (204): read the members, `delete_group`, recompute them, and record `GroupDeleted`.
8. **Lapse** (without `scim`):
   - refused (`forbidden`): `POST`, adding members, `ReplaceMembers` that adds anyone, and renames;
   - allowed: removing members, `ReplaceMembers` that only removes, and `DELETE`.

   Recompute then lowers only (P-5).

**Tests:**
- `create_with_members_maps_roles`, `list_filters_by_display_name`, `excluded_members_are_omitted`;
- `duplicate_display_name_is_409`, `another_orgs_group_is_404`;
- `patch_add_and_remove_recompute_roles`, `put_without_members_keeps_them`;
- `delete_lowers_members_to_their_remaining_groups`;
- `lapsed_org_removes_members_but_cannot_add`;
- `every_group_operation_is_recorded`.

**Commit:** `Serve SCIM Groups`

---

### G4 — Mapping API

**Edit:**
- `crates/server/src/ee/scim/mappings.rs` (new);
- `crates/server/src/api/scim.rs`, `crates/server/src/api/openapi.rs`;
- `crates/server/tests/openapi.rs`, `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs`;
- `fixtures/api/static/org-scim-groups.json` (new) and the static JSON of the write responses;
- `web/src/mocks/writes.ts`.

**Steps:**

1. **DTOs:**

   ```rust
   pub struct ScimGroup { pub id: Uuid, pub display_name: String, pub external_id: Option<String>, pub member_count: u32, pub mapping: Option<ScimGroupMapping> }
   pub struct ScimGroupMapping { pub org_role: Option<OrgRole>, pub projects: Vec<ScimProjectRole> }
   pub struct ScimProjectRole { pub project_id: Uuid, pub role: ProjectRoleOverride } // reuse api::members::ProjectRoleOverride
   pub struct PutScimGroupMapping { pub org_role: Option<OrgRole>, pub projects: Vec<ScimProjectRole> } // deny_unknown_fields
   pub struct ScimGroupList { pub groups: Vec<ScimGroup> }
   ```

   The display name is shown to owners only, on their own settings page; it is never logged or recorded.
2. **Routes**, `Access::Org { perm: OrgPerm::ManageProvisioning, session_only: true }`, documented:

   | Route | Behaviour | Operation id |
   |---|---|---|
   | `GET /api/v1/orgs/{org}/scim/groups` | `ScimGroupList` of every group (`all_groups`, `member_counts`, `mappings`), ordered by display name | `list_org_scim_groups` |
   | `PUT /api/v1/orgs/{org}/scim/groups/{id}/mapping` | set the mapping (see below); 200 + `ScimGroup` | `put_org_scim_group_mapping` |
   | `DELETE /api/v1/orgs/{org}/scim/groups/{id}/mapping` | remove the mapping; always allowed; 204, or 404 for an unknown group | `delete_org_scim_group_mapping` |

   **PUT:**
   - Validation:
     - `org_role = owner` is a 400 `field_error("org_role", "owner cannot be mapped")`;
     - at most 200 projects; a duplicate `project_id` is a 400 `field_error("projects", "duplicate project")`;
     - inside the write: the group belongs to the org (`group_tx(conn, org, id)`, 404 otherwise, P-7) and each project belongs to it (`SELECT id FROM projects WHERE org_id = ? AND id IN (…)`, else 400 `field_error("projects", "unknown project")`);
     - an empty mapping (no org role, no projects) is a 400: use DELETE.
   - Converters between `ProjectOverride` and `ProjectRoleOverride` are written in `mappings.rs` (the existing ones are private in `ee/project_roles.rs` and `routes/members.rs`).
   - Gating: needs `scim`, except when the change only lowers roles (every new value is lower than or equal to the old one, and no new rows). Writing project roles needs `project_roles` when any project row is new or higher.
   - In one write: `put_mapping`, then, when SCIM is enabled (P-6), `recompute_tx(group members)`, then record `GroupMappingChanged { org_role, projects }`. The actor is the owner. DELETE does the same with `delete_mapping`.
3. **OpenAPI:**
   - the three operations (tag `orgs`, security `session`) and the DTO schemas;
   - `tests/openapi.rs`: the ids in `OPERATIONS`, the session list, and `expected_errors` copied from the SCIM settings operations;
   - `api_examples.rs`: `list_org_scim_groups` → `static/org-scim-groups.json` (a `ScimGroupList` with two groups, one mapped);
   - `tests/api_examples.rs`: its round-trip arm, and `roundtrip::<ScimGroup>("static/scim-group.json")` for the PUT response file;
   - `writes.ts`: `put_org_scim_group_mapping` (responds with `fixtures/api/static/scim-group.json`, new) and `delete_org_scim_group_mapping` (204) in both lists.

**Tests:**
- `owner_maps_a_group_and_roles_follow`;
- `owner_cannot_be_mapped`, `projects_of_another_org_are_refused`;
- `lowering_works_when_lapsed`, `raising_needs_the_feature`;
- `project_roles_need_the_project_roles_feature`;
- `delete_mapping_returns_members_to_the_default`;
- `admins_cannot_manage_mappings`;
- `another_orgs_group_is_404`, `duplicate_projects_are_refused`;
- `mapping_changes_with_scim_off_recompute_nothing`.

**Commit:** `Add the SCIM group mapping API`

---

### G5 — Okta and Entra ID fixtures, replayed

**Edit:** `fixtures/scim/README.md` (new), `fixtures/scim/okta/*.json` and `fixtures/scim/entra/*.json` (new), `crates/server/src/ee/scim/replay_tests.rs` (new; G1a already declared it in `mod.rs`).

**Steps:**

1. **`fixtures/scim/README.md`:** the files are requests in the shape Okta's and Entra ID's SCIM clients send, written from their public documentation. They are not captured from a validator run; replacing them with captured requests is a follow-up. Each file is one ordered scenario.
2. **Scenario files** are a JSON array of steps:

   ```json
   [{ "method": "POST", "path": "/scim/v2/Users", "body": { … }, "expect": { "status": 201, "save": { "user": "/id" } } },
    { "method": "PATCH", "path": "/scim/v2/Users/{user}", "body": { … }, "expect": { "status": 200 } }]
   ```

   - `save` stores a JSON pointer of the response under a name.
   - `{name}` in later paths and bodies is substituted by plain text replacement on the serialized step before parsing it. Query strings are percent-encoded by the test runner (`filter` values contain spaces and quotes).
   - `expect` may also hold `"json": { "<pointer>": <value> }` checks.
3. **Scenarios** (emails use the test domain `acme.test`):

   | Scenario | Steps |
   |---|---|
   | `okta/connection.json` | `GET /Users?startIndex=1&count=1` → 200 |
   | `okta/user_lifecycle.json` | `GET /Users?filter=userName eq "…"` (empty); POST; GET; PUT with the profile; PATCH `{"op":"replace","value":{"active":false}}`; PATCH active true |
   | `okta/groups.json` | POST group; PATCH `{"op":"add","path":"members","value":[{"value":"{user}"}]}`; PATCH `{"op":"replace","value":{"id":"{group}","displayName":"Renamed"}}`; PATCH `remove` with path `members[value eq "{user}"]`; DELETE |
   | `entra/connection.json` | `GET /Users?filter=userName eq "…"` (random, empty) |
   | `entra/user_lifecycle.json` | POST with an enterprise extension and a different `emails[work]` (D-EA-13 ignores it); PATCH `{"op":"Replace","path":"active","value":"False"}`; PATCH `name.givenName` and the enterprise `department` path; DELETE |
   | `entra/groups.json` | `GET /Groups?excludedAttributes=members&filter=displayName eq "…"`; POST; PATCH `{"op":"Add","path":"members","value":[{"value":"{user}"}]}`; PATCH `{"op":"Remove","path":"members","value":[{"value":"{user}"}]}` |

4. **`replay_tests.rs`:**
   - one test per scenario, loading the file with `include_str!("../../../../../fixtures/scim/<vendor>/<name>.json")`;
   - a licensed org with SCIM enabled, a verified domain `acme.test` and a token, through `ee::scim::test_helpers::scim_org` (G1a);
   - each step is sent with `content-type: application/scim+json`, then status, saves and JSON checks are asserted.
5. **`AGENTS.md` § Layout:** not in the list. Report that a `fixtures/scim/` line belongs there, and the coordinator adds it.

**Commit:** `Replay Okta and Entra ID SCIM requests in tests`

---

### G6 — Role mappings card

**Prerequisite:** the coordinator ran `just openapi` and `just api-examples` after G4.

**Edit:** `web/src/ee/scim/mappings-section.tsx` (new, SPDX) and its test, `web/src/ee/scim/provisioning-settings.tsx`, `web/src/ee/scim/tokens-section.tsx` (a revoke-only mode), `web/src/api/queries-settings.ts` (`orgScimGroupsQuery`), `web/src/routes/$org/settings/provisioning.test.tsx`.

**Steps:**

1. **`MappingsSection`** (`SettingsCard` "Role mappings", description "Groups your identity provider pushes, and the roles they give."):
   - A table: group name, members, org role, projects. An empty state: "Your identity provider has not pushed any group yet."
   - Edit opens a panel with:
     - an org role select (Not mapped, Viewer, Member, Admin);
     - project rows: project select from `useInfiniteQuery(projectListOptions(org))` (`api/queries.ts`; load every page, as orgs can have more than 50 projects), role select (No access, Viewer, Member, Admin, via `overrideLabel`), and remove; plus "Add project";
     - Save (PUT) and Remove mapping (DELETE).
   - A line: "Owners keep their role. Admins get no project roles. Members with no mapped group get the default role."
   - Errors via `problemMessage`.
2. **`ProvisioningSettings`** renders it after the tokens section. Fix C6's gap here too: when `scim` is locked but SCIM is enabled or tokens exist, show the lock, a "Turn off SCIM" button and the tokens card with Revoke only. The server allows both when lapsed (D-EA-19).

**Tests:**
- `lists groups with their mappings`, `maps a group to member and a project role`, `removes a mapping`, `empty state`;
- `lapsed org can still turn SCIM off and revoke tokens`.

**Commit:** `Add the SCIM role mappings card`

---

### G7 — Docs

**Edit:** `website/src/content/docs/docs/platform/scim.mdx`, `website/src/content/docs/docs/platform/scim-okta.mdx` and `scim-entra.mdx` (new), `website/astro.config.mjs`, `docs/spec/04-metadata-db.md`, `docs/spec/08-auth.md`, `docs/spec/09-api.md`, `docs/spec/11-ui.md`, `docs/plans/ea-enterprise-admin/README.md` (P-2/P-3 notes in § Roles from groups; § Milestones M6 says fixtures are documentation-based), `docs/mvp-decisions.md` (§ Track EA), `AGENTS.md` (§ Layout: `fixtures/scim/`), `CHANGELOG.md`.

**Decisions to record** (design README decision table and `docs/mvp-decisions.md` § Track EA, in their row format, with rejected alternatives):
- **D-EA-24 (P-2):** every change that can affect a managed member's roles recomputes them in its transaction; a linked existing member is recomputed at once (replaces M5's "SCIM never changes an existing role"). Rejected: recompute on group changes only.
- **D-EA-25 (P-3, P-6):** a default-role change recomputes unmapped managed members; recomputes run only while SCIM is enabled. Rejected: the default role affects new members only.
- **D-EA-26 (P-5):** raises and lowerings are judged by effective access before and after, so removing a `none` override is a raise. Rejected: rank of the stored override.
- **D-EA-27 (M5 fix):** cetra's account email is `userName`; incoming `emails` are ignored, never refused (Entra's UPN and mail often differ). Rejected: refuse a mismatch.
- **D-EA-28 (M5 fix):** an account SCIM disabled is re-enabled when the IdP creates or reactivates the user again, even after the SCIM row was deleted (`scim_disabled_accounts`); an account an instance admin disabled stays refused. Rejected: refuse every disabled account.

Add their ids to nothing else; the coordinator updates the roadmap row.

**Steps:**
1. **`scim.mdx`:**
   - § Groups: the resource, filters and PATCH forms.
   - § Roles from groups: mappings, highest wins, owners, admins and project roles, the default role and its recompute, seats and `scim.role_raise_refused`, lapses.
   - Update the line saying Groups are unsupported, and the line saying provisioning never changes an existing member's role.
2. **`scim-okta.mdx`:** create the app (SCIM 2.0, base URL, unique identifier `userName`, the bearer token); enable push of new users, profile updates, deactivation and groups; push groups; map them in cetra.
3. **`scim-entra.mdx`:** create the enterprise app, set provisioning to Automatic, set Tenant URL = base URL and Secret token; keep the default attribute mappings (`userName` ← `userPrincipalName`; `emails` is ignored); assign users and groups; map the groups in cetra.

   Both guides state that the requests were checked against documented shapes, not a vendor validator run.
4. **Sidebar:** the two guides after "SCIM provisioning".
5. **Specs:**
   - 04: the four tables.
   - 08: Groups, recompute and seats.
   - 09: the mapping routes and the Groups surface.
   - 11: the card.
6. **`CHANGELOG.md` `Added`:** "SCIM groups and role mappings to org and project roles, with Okta and Entra ID setup guides."

**Commit:** `Document SCIM groups and the Okta and Entra ID setup`

---

## After the phases (coordinating session)

1. Update `meta_schema.snap` (`INSTA_UPDATE=always`).
2. Add golden rows for the four tables in `golden_tests/build.rs`, through a Haiku phase.
3. Run `just openapi` and `just api-examples` after G4 (the manifest comes from the latter).
4. Run `just check` once; fix phase; Sonnet code review of M5 and M6 together.
5. Roadmap: "M6 implemented".
