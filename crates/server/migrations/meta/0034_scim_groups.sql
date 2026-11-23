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
