import type { Schema } from "@/api/types";
import { type PillTone, StatusPill } from "@/components/settings/status-pill";

const LABEL: Record<Schema<"IssueState">, string> = {
  open: "Open",
  resolved: "Resolved",
  ignored: "Ignored",
  merged: "Merged",
};

/** Open needs attention (error), resolved is done (ok), ignored is muted, merged points elsewhere. */
const TONE: Record<Schema<"IssueState">, PillTone> = {
  open: "error",
  resolved: "ok",
  ignored: "unset",
  merged: "info",
};

export function IssueStateBadge({ state }: { state: Schema<"IssueState"> }) {
  return <StatusPill tone={TONE[state]}>{LABEL[state]}</StatusPill>;
}
