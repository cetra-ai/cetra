import {
  BellRing,
  Database,
  FileText,
  FlaskConical,
  Gauge,
  LayoutDashboard,
  ListChecks,
  MessagesSquare,
  Rows3,
  Table2,
  Users,
  Waypoints,
} from "lucide-react";

export const NAV_SECTIONS = ["Observe", "Monitor", "Evaluate", "Explore"] as const;

export type NavSection = (typeof NAV_SECTIONS)[number];

/** Project pages that exist, ordered by section. The go key lives in shortcuts.ts (goKeyOf). */
export const projectNav = [
  { section: "Observe", to: "/$org/$project/dashboard", label: "Dashboard", icon: LayoutDashboard },
  { section: "Observe", to: "/$org/$project/traces", label: "Traces", icon: Waypoints },
  { section: "Observe", to: "/$org/$project/spans", label: "Spans", icon: Rows3 },
  { section: "Observe", to: "/$org/$project/sessions", label: "Sessions", icon: MessagesSquare },
  { section: "Observe", to: "/$org/$project/users", label: "Users", icon: Users },
  { section: "Monitor", to: "/$org/$project/alerts", label: "Alerts", icon: BellRing },
  { section: "Evaluate", to: "/$org/$project/prompts", label: "Prompts", icon: FileText },
  { section: "Evaluate", to: "/$org/$project/evaluators", label: "Evaluators", icon: Gauge },
  {
    section: "Evaluate",
    to: "/$org/$project/online-rules",
    label: "Online rules",
    icon: ListChecks,
  },
  { section: "Evaluate", to: "/$org/$project/datasets", label: "Datasets", icon: Table2 },
  {
    section: "Evaluate",
    to: "/$org/$project/experiments",
    label: "Experiments",
    icon: FlaskConical,
  },
  { section: "Explore", to: "/$org/$project/sql", label: "SQL", icon: Database },
] as const;
