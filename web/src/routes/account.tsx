import { useSuspenseQuery } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { authConfigQuery } from "@/api/auth";
import { meQuery } from "@/api/queries";
import { PasswordForm } from "@/components/account/password-form";
import { PERSONAL_TOKEN_SORTS, PersonalTokens } from "@/components/account/personal-tokens";
import { ProfileForm } from "@/components/account/profile-form";
import { SettingsPage } from "@/components/settings/settings-page";
import { TopBarLayout } from "@/components/shell/top-bar-layout";
import { parseSettingsListSearch } from "@/lib/search/settings-list";

export const Route = createFileRoute("/account")({
  validateSearch: (search) => parseSettingsListSearch(search, PERSONAL_TOKEN_SORTS),
  loader: ({ context }) => context.queryClient.ensureQueryData(authConfigQuery),
  component: AccountPage,
});

function AccountPage() {
  const { data: config } = useSuspenseQuery(authConfigQuery);
  const { data: me } = useSuspenseQuery(meQuery);
  const search = Route.useSearch();
  const navigate = Route.useNavigate();
  // With password sign-in off, only instance admins keep a password (D-S-3).
  const showPassword = config.password_enabled || me.user.is_instance_admin;
  return (
    <TopBarLayout>
      <main className="flex min-h-0 flex-1 flex-col">
        <SettingsPage title="Account" description="Your profile, password and access tokens.">
          <ProfileForm />
          {showPassword ? <PasswordForm /> : null}
          <PersonalTokens
            search={search}
            onSearchChange={(patch) => {
              void navigate({ search: (prev) => ({ ...prev, ...patch }), replace: true });
            }}
          />
        </SettingsPage>
      </main>
    </TopBarLayout>
  );
}
