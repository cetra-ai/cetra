import { Moon, Sun } from "lucide-react";
import { useTheme } from "@/components/theme-provider";
import { restoreSavedPaletteOnClose, ThemeMenuItems } from "@/components/theme-toggle";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { SidebarMenuButton, SidebarMenuItem, useSidebar } from "@/components/ui/sidebar";

/**
 * The sidebar footer's Theme entry, above Settings: theme, palette and density. Collapsed, it is
 * the icon with a tooltip.
 */
export function SidebarTheme() {
  const { isMobile, state } = useSidebar();
  const { resolvedTheme } = useTheme();
  const collapsed = state === "collapsed" && !isMobile;
  const ThemeIcon = resolvedTheme === "dark" ? Moon : Sun;
  return (
    <SidebarMenuItem>
      <DropdownMenu onOpenChange={restoreSavedPaletteOnClose}>
        <DropdownMenuTrigger asChild>
          <SidebarMenuButton tooltip="Theme">
            <ThemeIcon aria-hidden="true" />
            <span>Theme</span>
          </SidebarMenuButton>
        </DropdownMenuTrigger>
        <DropdownMenuContent
          side={collapsed || !isMobile ? "right" : "top"}
          align="end"
          className="w-48"
        >
          <ThemeMenuItems />
        </DropdownMenuContent>
      </DropdownMenu>
    </SidebarMenuItem>
  );
}
