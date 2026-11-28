import type { ReactNode } from "react";
import { DemoBar } from "@/components/shell/demo-bar";
import { TopBar } from "@/components/shell/top-bar";
import { TopBarSlotProvider } from "@/components/shell/top-bar-slot";

/** Full-height page with the top bar. Children fill the rest and scroll themselves. */
export function TopBarLayout({ children }: { children: ReactNode }) {
  return (
    <TopBarSlotProvider>
      <div className="flex h-dvh flex-col overflow-hidden">
        {import.meta.env.MODE === "demo" ? <DemoBar /> : null}
        <TopBar />
        <div className="flex min-h-0 flex-1 flex-col">{children}</div>
      </div>
    </TopBarSlotProvider>
  );
}
