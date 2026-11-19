import { BookOpen, ChevronsUpDown, ExternalLink } from "lucide-react";
import { $api } from "@/api/client";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  useSidebar,
} from "@/components/ui/sidebar";
import {
  initials,
  UserMenuAccountLinks,
  UserMenuHeader,
  UserMenuSignOutItem,
  UserMenuTimeZoneItem,
} from "@/components/user-menu";
import { APP_VERSION, DOCS_URL } from "@/product";

/**
 * Account footer for the project sidebar: the user's menu, and the docs link with the version.
 * Icon mode (collapsed sidebar) shows the avatar and a docs icon only.
 */
export function SidebarAccount() {
  const { isMobile, state } = useSidebar();
  const { data: me } = $api.useSuspenseQuery("get", "/api/auth/me");
  const name = me.user.name?.trim() ? me.user.name : undefined;
  const label = name ?? me.user.email;
  const letters = initials(me.user.name, me.user.email);
  const collapsed = state === "collapsed" && !isMobile;

  return (
    <SidebarMenu>
      <SidebarMenuItem>
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <SidebarMenuButton size="lg" tooltip={label} aria-label={label}>
              <Avatar size="sm">
                <AvatarFallback>{letters}</AvatarFallback>
              </Avatar>
              <span className="grid min-w-0 flex-1 text-left leading-tight group-data-[collapsible=icon]:hidden">
                <span className="truncate font-medium">{label}</span>
                {name ? (
                  <span className="truncate text-caption text-muted-foreground">
                    {me.user.email}
                  </span>
                ) : null}
              </span>
              <ChevronsUpDown className="ml-auto group-data-[collapsible=icon]:hidden" />
            </SidebarMenuButton>
          </DropdownMenuTrigger>
          <DropdownMenuContent
            side={collapsed || !isMobile ? "right" : "top"}
            align="end"
            className="min-w-56"
          >
            <UserMenuHeader />
            <DropdownMenuSeparator />
            <UserMenuAccountLinks />
            <DropdownMenuSeparator />
            <UserMenuTimeZoneItem />
            <DropdownMenuSeparator />
            <UserMenuSignOutItem />
          </DropdownMenuContent>
        </DropdownMenu>
      </SidebarMenuItem>
      <SidebarMenuItem>
        {collapsed ? (
          <SidebarMenuButton asChild tooltip={`Docs · v${APP_VERSION}`}>
            <a href={DOCS_URL} target="_blank" rel="noreferrer" aria-label="Docs">
              <BookOpen aria-hidden="true" />
            </a>
          </SidebarMenuButton>
        ) : (
          <div className="flex items-center justify-between gap-2 px-2 text-caption text-muted-foreground">
            <a
              href={DOCS_URL}
              target="_blank"
              rel="noreferrer"
              className="inline-flex items-center gap-1 rounded-sm outline-none hover:text-foreground focus-visible:outline-2 focus-visible:outline-ring focus-visible:outline-offset-2 focus-visible:outline-solid"
            >
              <BookOpen aria-hidden="true" className="size-3" />
              Docs
              <ExternalLink aria-hidden="true" className="size-3" />
            </a>
            <span className="font-mono text-caption text-muted-foreground">v{APP_VERSION}</span>
          </div>
        )}
      </SidebarMenuItem>
    </SidebarMenu>
  );
}
