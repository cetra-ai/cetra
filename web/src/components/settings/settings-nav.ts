export const orgSettingsNav = [
  { to: "/$org/settings/general", label: "General", ownerOnly: false },
  { to: "/$org/settings/plan", label: "Plan", ownerOnly: false },
  { to: "/$org/settings/members", label: "Members", ownerOnly: false },
  { to: "/$org/settings/llm-connections", label: "LLM connections", ownerOnly: false },
  { to: "/$org/settings/models", label: "Model prices", ownerOnly: false },
  { to: "/$org/settings/sso", label: "Single sign-on", ownerOnly: true },
] as const;

export const projectSettingsNav = [
  { to: "/$org/$project/settings/general", label: "General", adminOnly: false },
  { to: "/$org/$project/settings/api-keys", label: "API keys", adminOnly: true },
  { to: "/$org/$project/settings/data", label: "Data", adminOnly: true },
  { to: "/$org/$project/settings/members", label: "Members", adminOnly: false },
  { to: "/$org/$project/settings/score-definitions", label: "Score definitions", adminOnly: false },
  { to: "/$org/$project/settings/models", label: "Model prices", adminOnly: false },
  { to: "/$org/$project/settings/channels", label: "Channels", adminOnly: false },
] as const;

/** True on `/<org>/<project>/settings` and every path below it. Slugs are `[a-z0-9-]`. */
export function isProjectSettingsPath(pathname: string, org: string, project: string): boolean {
  const base = `/${org}/${project}/settings`;
  return pathname === base || pathname.startsWith(`${base}/`);
}
