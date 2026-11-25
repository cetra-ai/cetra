// SPDX-License-Identifier: Elastic-2.0
import { useInfiniteQuery, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { projectListOptions } from "@/api/queries";
import { orgScimGroupsQuery } from "@/api/queries-settings";
import type { Schema } from "@/api/types";
import { FormError } from "@/components/form/form-error";
import { SettingsCard } from "@/components/settings/settings-card";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { Skeleton } from "@/components/ui/skeleton";
import { overrideLabel, roleLabel } from "@/lib/roles";
import { uuidv7 } from "@/lib/uuid";

type Group = Schema<"ScimGroup">;
type Mapping = Schema<"ScimGroupMapping">;
type Project = Schema<"Project">;
type ProjectRole = Schema<"ProjectRoleOverride">;

/** The org role choices of a mapping; `none` stores no org role (the API omits it). */
const ORG_ROLE_CHOICES = ["none", "viewer", "member", "admin"] as const;
type OrgRoleChoice = (typeof ORG_ROLE_CHOICES)[number];

const PROJECT_ROLES: ProjectRole[] = ["admin", "member", "viewer", "none"];

/** One project row of the editor. `key` only keeps React rows apart; it is never sent. */
type Row = { key: string; projectId: string; role: ProjectRole };

/**
 * The SCIM groups of the org with their role mappings (D-EA-14). Owners edit a group's mapping
 * in a panel; the groups and their members are the identity provider's, never edited here.
 */
export function MappingsSection({ org }: { org: string }) {
  const groups = useQuery(orgScimGroupsQuery(org));
  const projects = useAllProjects(org);
  const [editing, setEditing] = useState<Group | undefined>();

  return (
    <SettingsCard
      title="Role mappings"
      description="Groups your identity provider pushes, and the roles they give."
    >
      {groups.isPending ? <Skeleton className="h-32 w-full" /> : null}
      {!groups.isPending && groups.data === undefined ? (
        <FormError message={problemMessage(groups.error, "Could not load the groups.")} />
      ) : null}
      {groups.data === undefined ? null : (
        <GroupTable groups={groups.data.groups} projects={projects} onEdit={setEditing} />
      )}
      <p className="text-caption text-muted-foreground">
        Owners keep their role. Admins get no project roles. Members with no mapped group get the
        default role.
      </p>
      <Sheet
        open={editing !== undefined}
        onOpenChange={(open) => {
          if (!open) {
            setEditing(undefined);
          }
        }}
      >
        <SheetContent className="w-full overflow-y-auto sm:max-w-md">
          {editing === undefined ? null : (
            <MappingEditor
              key={editing.id}
              org={org}
              group={editing}
              projects={projects}
              onDone={() => {
                setEditing(undefined);
              }}
            />
          )}
        </SheetContent>
      </Sheet>
    </SettingsCard>
  );
}

/** Every project of the org, all pages: the list is paged, and orgs can have more than 50 projects. */
function useAllProjects(org: string): Project[] {
  const query = useInfiniteQuery(projectListOptions(org));
  const { hasNextPage, isFetchingNextPage, fetchNextPage } = query;
  useEffect(() => {
    if (hasNextPage && !isFetchingNextPage) {
      void fetchNextPage();
    }
  }, [hasNextPage, isFetchingNextPage, fetchNextPage]);
  return (query.data?.pages ?? []).flatMap((page) => page.data);
}

function GroupTable({
  groups,
  projects,
  onEdit,
}: {
  groups: Group[];
  projects: Project[];
  onEdit: (group: Group) => void;
}) {
  if (groups.length === 0) {
    return (
      <p className="text-body text-muted-foreground">
        Your identity provider has not pushed any group yet.
      </p>
    );
  }
  const names = new Map(projects.map((project) => [project.id, project.name]));
  return (
    <table aria-label="SCIM groups" className="w-full text-left text-body">
      <thead className="text-caption text-muted-foreground">
        <tr>
          <th scope="col" className="py-1 pr-3 font-medium">
            Group
          </th>
          <th scope="col" className="py-1 pr-3 font-medium">
            Members
          </th>
          <th scope="col" className="py-1 pr-3 font-medium">
            Org role
          </th>
          <th scope="col" className="py-1 pr-3 font-medium">
            Projects
          </th>
          <th scope="col" className="py-1">
            <span className="sr-only">Actions</span>
          </th>
        </tr>
      </thead>
      <tbody>
        {groups.map((group) => (
          <tr key={group.id} className="border-t">
            <td className="py-2 pr-3">{group.display_name}</td>
            <td className="py-2 pr-3">{group.member_count}</td>
            <td className="py-2 pr-3">
              {group.mapping?.org_role ? roleLabel(group.mapping.org_role) : "Not mapped"}
            </td>
            <td className="py-2 pr-3">{projectsText(group.mapping, names)}</td>
            <td className="py-2 text-right">
              <Button
                type="button"
                variant="outline"
                aria-label={`Edit ${group.display_name}`}
                onClick={() => {
                  onEdit(group);
                }}
              >
                Edit
              </Button>
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/** "Support bot (Admin), Playground (Viewer)", or "None" when the group maps no project. */
function projectsText(mapping: Mapping | null | undefined, names: Map<string, string>): string {
  if (!mapping || mapping.projects.length === 0) {
    return "None";
  }
  return mapping.projects
    .map(
      (entry) =>
        `${names.get(entry.project_id) ?? "Unknown project"} (${overrideLabel(entry.role)})`,
    )
    .join(", ");
}

/** The panel that edits one group's mapping. It is keyed by the group, so its state starts fresh. */
function MappingEditor({
  org,
  group,
  projects,
  onDone,
}: {
  org: string;
  group: Group;
  projects: Project[];
  onDone: () => void;
}) {
  const queryClient = useQueryClient();
  const mapping = group.mapping ?? undefined;
  const [orgRole, setOrgRole] = useState<OrgRoleChoice>(
    ORG_ROLE_CHOICES.find((choice) => choice === mapping?.org_role) ?? "none",
  );
  const [rows, setRows] = useState<Row[]>(() =>
    (mapping?.projects ?? []).map((entry) => ({
      key: uuidv7(),
      projectId: entry.project_id,
      role: entry.role,
    })),
  );

  const invalidate = () => {
    void queryClient.invalidateQueries({ queryKey: orgScimGroupsQuery(org).queryKey });
    onDone();
  };
  const save = $api.useMutation("put", "/api/v1/orgs/{org}/scim/groups/{id}/mapping", {
    onSuccess: invalidate,
  });
  const remove = $api.useMutation("delete", "/api/v1/orgs/{org}/scim/groups/{id}/mapping", {
    onSuccess: invalidate,
  });
  const busy = save.isPending || remove.isPending;

  function updateRow(key: string, change: Partial<Pick<Row, "projectId" | "role">>) {
    setRows((current) => current.map((row) => (row.key === key ? { ...row, ...change } : row)));
  }

  return (
    <>
      <SheetHeader>
        <SheetTitle>{`Map ${group.display_name}`}</SheetTitle>
        <SheetDescription>Members of this group get the roles below.</SheetDescription>
      </SheetHeader>
      <form
        noValidate
        aria-label={`Mapping of ${group.display_name}`}
        className="flex flex-col gap-4 px-4 pb-4"
        onSubmit={(event) => {
          event.preventDefault();
          save.reset();
          save.mutate({
            params: { path: { org, id: group.id } },
            body: {
              org_role: orgRole === "none" ? undefined : orgRole,
              // A row with no project chosen maps nothing, so it is left out.
              projects: rows
                .filter((row) => row.projectId !== "")
                .map((row) => ({ project_id: row.projectId, role: row.role })),
            },
          });
        }}
      >
        <div className="flex flex-col gap-2">
          <Label htmlFor="mapping-org-role">Org role</Label>
          <Select
            value={orgRole}
            onValueChange={(value) => {
              const next = ORG_ROLE_CHOICES.find((choice) => choice === value);
              if (next) {
                setOrgRole(next);
              }
            }}
            disabled={busy}
          >
            <SelectTrigger id="mapping-org-role" className="w-fit">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {ORG_ROLE_CHOICES.map((choice) => (
                <SelectItem key={choice} value={choice}>
                  {choice === "none" ? "Not mapped" : roleLabel(choice)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>

        <div className="flex flex-col gap-2">
          <p className="text-caption text-muted-foreground">Projects</p>
          {rows.map((row, index) => {
            // A project already chosen in another row is not offered again.
            const taken = new Set(
              rows.filter((other) => other.key !== row.key).map((other) => other.projectId),
            );
            return (
              <div key={row.key} className="flex flex-wrap items-center gap-2">
                <Select
                  value={row.projectId}
                  onValueChange={(value) => {
                    updateRow(row.key, { projectId: value });
                  }}
                  disabled={busy}
                >
                  <SelectTrigger aria-label={`Project ${index + 1}`} className="w-44">
                    <SelectValue placeholder="Choose a project" />
                  </SelectTrigger>
                  <SelectContent>
                    {projects
                      .filter((project) => project.id === row.projectId || !taken.has(project.id))
                      .map((project) => (
                        <SelectItem key={project.id} value={project.id}>
                          {project.name}
                        </SelectItem>
                      ))}
                  </SelectContent>
                </Select>
                <Select
                  value={row.role}
                  onValueChange={(value) => {
                    const role = PROJECT_ROLES.find((choice) => choice === value);
                    if (role) {
                      updateRow(row.key, { role });
                    }
                  }}
                  disabled={busy}
                >
                  <SelectTrigger aria-label={`Role in project ${index + 1}`} className="w-40">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {PROJECT_ROLES.map((choice) => (
                      <SelectItem key={choice} value={choice}>
                        {overrideLabel(choice)}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <Button
                  type="button"
                  variant="ghost"
                  aria-label={`Remove project ${index + 1}`}
                  disabled={busy}
                  onClick={() => {
                    setRows((current) => current.filter((other) => other.key !== row.key));
                  }}
                >
                  Remove
                </Button>
              </div>
            );
          })}
          <div>
            <Button
              type="button"
              variant="outline"
              disabled={busy}
              onClick={() => {
                setRows((current) => [
                  ...current,
                  { key: uuidv7(), projectId: "", role: "viewer" },
                ]);
              }}
            >
              Add project
            </Button>
          </div>
        </div>

        <FormError
          message={
            save.error ? problemMessage(save.error, "Could not save the mapping.") : undefined
          }
        />
        <FormError
          message={
            remove.error ? problemMessage(remove.error, "Could not remove the mapping.") : undefined
          }
        />
        <div className="flex flex-wrap items-center justify-between gap-2">
          {mapping ? (
            <Button
              type="button"
              variant="outline"
              disabled={busy}
              onClick={() => {
                remove.reset();
                remove.mutate({ params: { path: { org, id: group.id } } });
              }}
            >
              Remove mapping
            </Button>
          ) : (
            <span />
          )}
          <Button type="submit" disabled={busy}>
            Save
          </Button>
        </div>
      </form>
    </>
  );
}
