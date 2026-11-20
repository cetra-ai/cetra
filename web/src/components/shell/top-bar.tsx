import { Link, useMatch, useRouterState } from "@tanstack/react-router";
import { AssistantButton } from "@/components/assistant/assistant-button";
import { CetraMark } from "@/components/brand/cetra-mark";
import { CommandPalette } from "@/components/command/palette";
import { OrgSwitcher } from "@/components/org-switcher";
import { isProjectSettingsPath } from "@/components/settings/settings-nav";
import { AlertBell } from "@/components/shell/alert-bell";
import { IngestDot } from "@/components/shell/ingest-dot";
import { pageOf } from "@/components/shell/page-crumb";
import { ProjectSwitcher } from "@/components/shell/project-switcher";
import { TopBarSlotTarget, TopBarStartTarget } from "@/components/shell/top-bar-slot";
import { ThemeToggle } from "@/components/theme-toggle";
import { UserMenu } from "@/components/user-menu";
import { PRODUCT_NAME } from "@/product";

const SEPARATOR_CLASS = "text-muted-foreground/60";

export function TopBar() {
  const inOrg = useMatch({ from: "/$org", shouldThrow: false }) !== undefined;
  const project = useMatch({ from: "/$org/$project", shouldThrow: false });
  const pathname = useRouterState({ select: (state) => state.location.pathname });
  // The project sidebar carries the theme and account menus, except on project settings pages.
  const sidebarHasAccount =
    project !== undefined &&
    !isProjectSettingsPath(pathname, project.params.org, project.params.project);
  const page =
    project === undefined
      ? undefined
      : pageOf(pathname, project.params.org, project.params.project);
  return (
    <header className="flex h-[40px] shrink-0 items-center gap-2 border-b px-3">
      <TopBarStartTarget />
      <nav aria-label="Breadcrumb" className="flex min-w-0 items-center gap-1.5">
        <Link
          to="/"
          className="rounded-sm font-medium outline-none focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
        >
          <span className="inline-flex items-center gap-1.5">
            <CetraMark />
            {PRODUCT_NAME}
          </span>
        </Link>
        {inOrg ? (
          <>
            <span aria-hidden="true" className={SEPARATOR_CLASS}>
              /
            </span>
            <OrgSwitcher />
            <span aria-hidden="true" className={SEPARATOR_CLASS}>
              /
            </span>
            <ProjectSwitcher />
          </>
        ) : null}
        {page ? (
          <>
            <span aria-hidden="true" className={SEPARATOR_CLASS}>
              /
            </span>
            <h1 className="truncate text-body font-medium">{page.label}</h1>
            <TopBarSlotTarget />
          </>
        ) : null}
      </nav>
      <div className="flex-1" />
      {project ? (
        <>
          <AssistantButton org={project.params.org} project={project.params.project} />
          <IngestDot org={project.params.org} project={project.params.project} />
          <AlertBell org={project.params.org} project={project.params.project} />
        </>
      ) : null}
      {inOrg ? <CommandPalette /> : null}
      {sidebarHasAccount ? null : (
        <>
          <ThemeToggle />
          <UserMenu />
        </>
      )}
    </header>
  );
}
