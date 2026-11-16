import { useSuspenseQuery } from "@tanstack/react-query";
import { createFileRoute } from "@tanstack/react-router";
import { authConfigQuery } from "@/api/auth";
import { meQuery } from "@/api/queries";
import { PasswordForm } from "@/components/account/password-form";
import { PersonalTokens } from "@/components/account/personal-tokens";
import { ProfileForm } from "@/components/account/profile-form";
import { TopBarLayout } from "@/components/shell/top-bar-layout";

export const Route = createFileRoute("/account")({
  loader: ({ context }) => context.queryClient.ensureQueryData(authConfigQuery),
  component: AccountPage,
});

function AccountPage() {
  const { data: config } = useSuspenseQuery(authConfigQuery);
  const { data: me } = useSuspenseQuery(meQuery);
  // With password sign-in off, only instance admins keep a password (D-S-3).
  const showPassword = config.password_enabled || me.user.is_instance_admin;
  return (
    <TopBarLayout>
      <main className="mx-auto flex w-full max-w-3xl flex-1 flex-col gap-10 overflow-auto px-4 py-6">
        <h1 className="text-2xl font-medium">Account</h1>
        <section className="flex flex-col gap-3" aria-labelledby="profile-heading">
          <h2 id="profile-heading" className="text-lg font-medium">
            Profile
          </h2>
          <ProfileForm />
        </section>
        {showPassword ? (
          <section className="flex flex-col gap-3" aria-labelledby="password-heading">
            <h2 id="password-heading" className="text-lg font-medium">
              Password
            </h2>
            <PasswordForm />
          </section>
        ) : null}
        <section className="flex flex-col gap-3" aria-labelledby="tokens-heading">
          <h2 id="tokens-heading" className="text-lg font-medium">
            Personal access tokens
          </h2>
          <PersonalTokens />
        </section>
      </main>
    </TopBarLayout>
  );
}
