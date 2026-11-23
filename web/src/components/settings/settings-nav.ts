import type { LucideIcon } from "lucide-react";
import {
  BellRing,
  Bot,
  Building2,
  CircleDollarSign,
  CreditCard,
  Gauge,
  KeyRound,
  LayoutGrid,
  LockKeyhole,
  Plug,
  Settings2,
  ShieldAlert,
  Users,
  UsersRound,
} from "lucide-react";

export type SettingsNavItem = {
  to: string;
  label: string;
  icon: LucideIcon;
  /** One line for the settings home cards and the command palette. */
  description: string;
  keywords: readonly string[];
};

export const orgSettingsNav = [
  {
    to: "/$org/settings",
    label: "Overview",
    icon: LayoutGrid,
    description: "Every organization setting in one place",
    keywords: ["home", "settings"],
    ownerOnly: false,
  },
  {
    to: "/$org/settings/general",
    label: "General",
    icon: Building2,
    description: "Organization name and URL",
    keywords: ["name", "slug", "delete"],
    ownerOnly: false,
  },
  {
    to: "/$org/settings/plan",
    label: "Plan",
    icon: CreditCard,
    description: "Plan, limits and usage",
    keywords: ["usage", "billing", "limits", "units"],
    ownerOnly: false,
  },
  {
    to: "/$org/settings/members",
    label: "Members",
    icon: Users,
    description: "Members and invites",
    keywords: ["invite", "roles", "owner"],
    ownerOnly: false,
  },
  {
    to: "/$org/settings/llm-connections",
    label: "LLM connections",
    icon: Plug,
    description: "Provider keys for LLM judges",
    keywords: ["openai", "anthropic", "judge", "provider"],
    ownerOnly: false,
  },
  {
    to: "/$org/settings/assistant",
    label: "Assistant",
    icon: Bot,
    description: "Turn on the in-app assistant and pick its model.",
    keywords: ["assistant", "ai", "chat", "copilot"],
    ownerOnly: false,
  },
  {
    to: "/$org/settings/models",
    label: "Model prices",
    icon: CircleDollarSign,
    description: "Organization token prices",
    keywords: ["cost", "pricing"],
    ownerOnly: false,
  },
  {
    to: "/$org/settings/sso",
    label: "Single sign-on",
    icon: LockKeyhole,
    description: "Identity provider and domains",
    keywords: ["sso", "oidc", "saml", "domain"],
    ownerOnly: true,
  },
  {
    to: "/$org/settings/provisioning",
    label: "Provisioning",
    icon: UsersRound,
    description: "SCIM from your identity provider",
    keywords: ["scim", "okta", "entra", "provisioning"],
    ownerOnly: true,
  },
] as const;

export const projectSettingsNav = [
  {
    to: "/$org/$project/settings",
    label: "Overview",
    icon: LayoutGrid,
    description: "Every project setting in one place",
    keywords: ["home", "settings"],
    adminOnly: false,
  },
  {
    to: "/$org/$project/settings/general",
    label: "General",
    icon: Settings2,
    description: "Name, retention and online evaluation",
    keywords: ["name", "slug", "retention", "timeout"],
    adminOnly: false,
  },
  {
    to: "/$org/$project/settings/api-keys",
    label: "API keys",
    icon: KeyRound,
    description: "Keys for SDKs and OTLP exporters",
    keywords: ["token", "secret", "public", "ingest"],
    adminOnly: true,
  },
  {
    to: "/$org/$project/settings/data",
    label: "Data",
    icon: ShieldAlert,
    description: "Delete an end user's data (GDPR)",
    keywords: ["gdpr", "deletion", "privacy"],
    adminOnly: true,
  },
  {
    to: "/$org/$project/settings/members",
    label: "Members",
    icon: Users,
    description: "Project roles and overrides",
    keywords: ["roles", "access"],
    adminOnly: false,
  },
  {
    to: "/$org/$project/settings/score-definitions",
    label: "Score definitions",
    icon: Gauge,
    description: "Scores used by annotations",
    keywords: ["annotation", "categories"],
    adminOnly: false,
  },
  {
    to: "/$org/$project/settings/models",
    label: "Model prices",
    icon: CircleDollarSign,
    description: "Token prices used for cost",
    keywords: ["cost", "pricing"],
    adminOnly: false,
  },
  {
    to: "/$org/$project/settings/channels",
    label: "Channels",
    icon: BellRing,
    description: "Where alerts are delivered",
    keywords: ["webhook", "email", "slack", "notifications"],
    adminOnly: false,
  },
] as const;

/** True on `/<org>/<project>/settings` and every path below it. Slugs are `[a-z0-9-]`. */
export function isProjectSettingsPath(pathname: string, org: string, project: string): boolean {
  const base = `/${org}/${project}/settings`;
  return pathname === base || pathname.startsWith(`${base}/`);
}
