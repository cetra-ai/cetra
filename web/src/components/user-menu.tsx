import { Link } from "@tanstack/react-router";
import { LogOut, ShieldCheck, UserRound } from "lucide-react";
import { $api } from "@/api/client";
import { useSignOut } from "@/components/auth/use-sign-out";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { setTimeZone, useTimeZone } from "@/lib/settings";

export function initials(name: string | null | undefined, email: string): string {
  const trimmed = name?.trim() ?? "";
  const source = trimmed.length > 0 ? trimmed : (email.split("@")[0] ?? "");
  const words = source.split(/\s+/).filter((word) => word.length > 0);
  const first = words[0]?.[0] ?? "";
  const last = words.length > 1 ? (words[words.length - 1]?.[0] ?? "") : "";
  return `${first}${last}`.toUpperCase();
}

/** Name, email and the instance admin badge. Not clickable. */
export function UserMenuHeader() {
  const { data: me } = $api.useSuspenseQuery("get", "/api/auth/me");
  const name = me.user.name?.trim() ? me.user.name : undefined;

  return (
    <>
      {name ? <DropdownMenuLabel>{name}</DropdownMenuLabel> : null}
      <p className="px-2 py-1.5 text-body text-muted-foreground">{me.user.email}</p>
      {me.user.is_instance_admin ? (
        <div className="px-2 py-1.5">
          <Badge variant="secondary">Instance admin</Badge>
        </div>
      ) : null}
    </>
  );
}

/** Account, and Admin for instance admins. */
export function UserMenuAccountLinks() {
  const { data: me } = $api.useSuspenseQuery("get", "/api/auth/me");

  return (
    <>
      <DropdownMenuItem asChild>
        <Link to="/account">
          <UserRound aria-hidden="true" />
          Account
        </Link>
      </DropdownMenuItem>
      {me.user.is_instance_admin ? (
        <DropdownMenuItem asChild>
          <Link to="/admin/users">
            <ShieldCheck />
            Admin
          </Link>
        </DropdownMenuItem>
      ) : null}
    </>
  );
}

export function UserMenuTimeZoneItem() {
  const zone = useTimeZone();

  return (
    <DropdownMenuCheckboxItem
      checked={zone === "utc"}
      onCheckedChange={(checked) => {
        setTimeZone(checked === true ? "utc" : "local");
      }}
    >
      Show times in UTC
    </DropdownMenuCheckboxItem>
  );
}

export function UserMenuSignOutItem() {
  const { signOut, pending } = useSignOut();

  return (
    <DropdownMenuItem disabled={pending} onSelect={signOut}>
      <LogOut aria-hidden="true" />
      Sign out
    </DropdownMenuItem>
  );
}

export function UserMenu() {
  return (
    <DropdownMenu>
      <UserMenuTrigger />
      <DropdownMenuContent align="end" className="min-w-56">
        <UserMenuHeader />
        <DropdownMenuSeparator />
        <UserMenuTimeZoneItem />
        <DropdownMenuSeparator />
        <UserMenuAccountLinks />
        <DropdownMenuSeparator />
        <UserMenuSignOutItem />
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function UserMenuTrigger() {
  const { data: me } = $api.useSuspenseQuery("get", "/api/auth/me");
  const name = me.user.name?.trim() ? me.user.name : undefined;
  const letters = initials(me.user.name, me.user.email);

  return (
    <DropdownMenuTrigger asChild>
      <Button
        variant="ghost"
        size="icon"
        aria-label={name ?? me.user.email}
        className="rounded-full"
      >
        <Avatar size="sm">
          <AvatarFallback>{letters}</AvatarFallback>
        </Avatar>
      </Button>
    </DropdownMenuTrigger>
  );
}
