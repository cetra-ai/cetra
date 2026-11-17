import type { ReactNode } from "react";

/** Width of the settings content column. */
export const SETTINGS_MAX_WIDTH = "max-w-5xl";

/** The frame of every settings page. `SidebarInset` is already the page's `<main>`. */
export function SettingsPage({
  title,
  description,
  actions,
  children,
}: {
  title: string;
  description?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="flex min-h-0 w-full flex-1 flex-col overflow-auto">
      <div className={`mx-auto flex w-full ${SETTINGS_MAX_WIDTH} flex-col gap-6 px-6 py-6`}>
        <div className="flex items-start justify-between gap-4">
          <div className="flex flex-col gap-1">
            <h1 className="text-heading font-semibold">{title}</h1>
            {description ? <p className="text-body text-muted-foreground">{description}</p> : null}
          </div>
          {actions ? <div className="flex shrink-0 items-center gap-2">{actions}</div> : null}
        </div>
        {children}
      </div>
    </div>
  );
}
