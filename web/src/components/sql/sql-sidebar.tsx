import { useState } from "react";
import type { Schema } from "@/api/types";
import { SavedQueriesList } from "@/components/sql/saved-queries-list";
import { SchemaBrowser } from "@/components/sql/schema-browser";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { STORAGE_PREFIX } from "@/product";

export const SIDEBAR_TAB_KEY = `${STORAGE_PREFIX}sql.sidebar-tab`;

type SidebarTab = "schema" | "saved";

/** The last tab the user chose; "Schema" when none is stored. */
function readTab(): SidebarTab {
  try {
    return localStorage.getItem(SIDEBAR_TAB_KEY) === "saved" ? "saved" : "schema";
  } catch {
    return "schema";
  }
}

function writeTab(tab: SidebarTab): void {
  try {
    localStorage.setItem(SIDEBAR_TAB_KEY, tab);
  } catch {
    // Private mode: the choice still applies for this page.
  }
}

export function SqlSidebar({
  schema,
  failed,
  onInsert,
  viewId,
  onLoad,
}: {
  schema: Schema<"SchemaResponse"> | undefined;
  /** The schema request failed. */
  failed: boolean;
  onInsert: (text: string) => void;
  /** `search.view`: the loaded saved query. */
  viewId: string | undefined;
  /** Load a saved query into the console. */
  onLoad: (view: Schema<"SavedView">) => void;
}) {
  const [tab, setTab] = useState<SidebarTab>(readTab);

  return (
    <aside aria-label="SQL sidebar" className="flex w-64 shrink-0 flex-col border-r min-h-0">
      <Tabs
        value={tab}
        onValueChange={(value) => {
          const next: SidebarTab = value === "saved" ? "saved" : "schema";
          setTab(next);
          writeTab(next);
        }}
        className="flex min-h-0 flex-1 flex-col gap-0"
      >
        <TabsList className="m-2 mb-0 flex w-auto shrink-0">
          <TabsTrigger value="schema" className="text-body">
            Schema
          </TabsTrigger>
          <TabsTrigger value="saved" className="text-body">
            Saved
          </TabsTrigger>
        </TabsList>
        <TabsContent value="schema" className="flex min-h-0 flex-1 flex-col">
          <SchemaBrowser schema={schema} failed={failed} onInsert={onInsert} />
        </TabsContent>
        <TabsContent value="saved" className="min-h-0 flex-1 overflow-auto p-2">
          <SavedQueriesList viewId={viewId} onLoad={onLoad} />
        </TabsContent>
      </Tabs>
    </aside>
  );
}
