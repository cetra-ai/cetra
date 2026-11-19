import type { ReactNode } from "react";

export type PillTone = "ok" | "warn" | "error" | "info" | "unset";

// Literal class names, so Tailwind generates every tone.
const TONE_CLASSES: Record<PillTone, { border: string; bg: string; text: string; dot: string }> = {
  ok: {
    border: "border-status-ok/30",
    bg: "bg-status-ok/10",
    text: "text-status-ok",
    dot: "bg-status-ok",
  },
  warn: {
    border: "border-status-warn/30",
    bg: "bg-status-warn/10",
    text: "text-status-warn",
    dot: "bg-status-warn",
  },
  error: {
    border: "border-status-error/30",
    bg: "bg-status-error/10",
    text: "text-status-error",
    dot: "bg-status-error",
  },
  info: {
    border: "border-status-info/30",
    bg: "bg-status-info/10",
    text: "text-status-info",
    dot: "bg-status-info",
  },
  unset: {
    border: "border-status-unset/30",
    bg: "bg-status-unset/10",
    text: "text-status-unset",
    dot: "bg-status-unset",
  },
};

export function StatusPill({
  tone,
  children,
  title,
}: {
  tone: PillTone;
  children: ReactNode;
  title?: string;
}) {
  const classes = TONE_CLASSES[tone];
  return (
    <span
      title={title}
      className={`inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-caption whitespace-nowrap ${classes.border} ${classes.bg} ${classes.text}`}
    >
      <span aria-hidden="true" className={`size-1.5 rounded-full ${classes.dot}`} />
      {children}
    </span>
  );
}
