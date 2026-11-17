import { createFileRoute } from "@tanstack/react-router";
import { UserPlus } from "lucide-react";
import { type ReactNode, useState } from "react";
import { $api } from "@/api/client";
import { InviteDialog } from "@/components/settings/invite-dialog";
import { InvitePanel, OrgInvites } from "@/components/settings/org-invites";
import { MemberPanel, OrgMembers } from "@/components/settings/org-members";
import { SettingsListLayout } from "@/components/settings/settings-list";
import { SettingsPage } from "@/components/settings/settings-page";
import { Button } from "@/components/ui/button";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { atLeast } from "@/lib/roles";
import { parseSettingsListSearch, type SettingsListSearch } from "@/lib/search/settings-list";

const SORTS = ["name", "role", "joined"] as const;
const TABS = ["members", "invites"] as const;

export const Route = createFileRoute("/$org/settings/members")({
  validateSearch: (search) => parseSettingsListSearch(search, SORTS, TABS),
  component: OrgMembersPage,
});

function OrgMembersPage() {
  const { org } = Route.useParams();
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  const { data: me } = $api.useSuspenseQuery("get", "/api/auth/me");
  const role = me.orgs.find((item) => item.slug === org)?.role;
  // Invitations are for admins and owners only; anyone else sees the members alone.
  const admin = atLeast(role, "admin");
  const tab = admin && search.tab === "invites" ? "invites" : "members";
  const [inviting, setInviting] = useState(false);
  const openId = search.open;

  function update(change: Partial<SettingsListSearch>) {
    void navigate({ search: (prev) => ({ ...prev, ...change }), replace: true });
  }

  function openRow(id: string, replace = false) {
    void navigate({ search: (prev) => ({ ...prev, open: id }), replace });
  }

  function closePanel(ignoreBlocker = false) {
    void navigate({ search: (prev) => ({ ...prev, open: undefined }), ignoreBlocker });
  }

  function switchTab(next: "members" | "invites") {
    // A list's sort and open row belong to that list, so they reset with the tab.
    void navigate({
      search: (prev) => ({ q: prev.q, tab: next === "invites" ? next : undefined }),
      replace: true,
    });
  }

  let panel: ReactNode = null;
  if (openId !== undefined) {
    panel =
      tab === "invites" ? (
        <InvitePanel org={org} inviteId={openId} onClose={closePanel} />
      ) : (
        <MemberPanel
          org={org}
          userId={openId}
          actorRole={role}
          me={me.user.id}
          onClose={closePanel}
        />
      );
  }

  return (
    <SettingsListLayout open={openId !== undefined} onClose={() => closePanel()} panel={panel}>
      <SettingsPage
        title="Members"
        description="People in this organization and their roles."
        actions={
          admin ? (
            <Button
              type="button"
              onClick={() => {
                setInviting(true);
              }}
            >
              <UserPlus />
              Invite member
            </Button>
          ) : undefined
        }
      >
        {admin ? (
          <Tabs
            value={tab}
            onValueChange={(value) => {
              switchTab(value === "invites" ? "invites" : "members");
            }}
          >
            <TabsList>
              <TabsTrigger value="members">Members</TabsTrigger>
              <TabsTrigger value="invites">Invites</TabsTrigger>
            </TabsList>
          </Tabs>
        ) : null}
        {tab === "invites" ? (
          <OrgInvites org={org} search={search} onSearchChange={update} onOpen={openRow} />
        ) : (
          <OrgMembers
            org={org}
            me={me.user.id}
            search={search}
            onSearchChange={update}
            onOpen={openRow}
          />
        )}
        <InviteDialog org={org} actorRole={role} open={inviting} onOpenChange={setInviting} />
      </SettingsPage>
    </SettingsListLayout>
  );
}
