import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { cn } from "cn";
import { useState } from "react";
import { problemOf } from "@/api/problem";
import { spanDetailQuery } from "@/api/queries";
import type { Schema } from "@/api/types";
import { useProject } from "@/components/shell/project-context";
import { AnnotateTab } from "@/components/trace/annotate-tab";
import { AttributesTab } from "@/components/trace/attributes-tab";
import { ChatTab } from "@/components/trace/chat-tab";
import { IoTab } from "@/components/trace/io-tab";
import { ScoresTab } from "@/components/trace/scores-tab";
import { type HeaderSpan, SpanHeader } from "@/components/trace/span-header";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { TooltipProvider } from "@/components/ui/tooltip";
import { useDelayedFlag } from "@/lib/use-delayed-flag";
import { STORAGE_PREFIX } from "@/product";

const TAB_KEY = `${STORAGE_PREFIX}span-tab`;
type SpanTab = "chat" | "io" | "attributes" | "scores" | "annotate";

export function SpanDetail({
  traceId,
  spanId,
  summary,
  detached = false,
  onAddFilter,
}: {
  traceId: string;
  spanId: string;
  /** The selected tree row's span. The header shows it while the detail of a new span loads. */
  summary?: Schema<"SpanSummary">;
  detached?: boolean;
  onAddFilter?: (conjunct: string) => void;
}) {
  const { org, project } = useProject();
  // The previous span's body stays on screen while the next one loads, so switching does not flash.
  const span = useQuery({
    ...spanDetailQuery(org, project, traceId, spanId),
    placeholderData: keepPreviousData,
  });
  const placeholder = span.isPlaceholderData;
  const slow = useDelayedFlag(placeholder);
  // A first load (no previous span to keep) shows the header from the tree at once and the
  // skeleton only if it takes long, so a quick load does not flash.
  const slowFirstLoad = useDelayedFlag(span.isPending);
  const [choice, setChoice] = useState(readStoredTab);
  function select(value: string) {
    if (!isSpanTab(value)) return;
    setChoice(value);
    localStorage.setItem(TAB_KEY, value);
  }

  if (span.isPending) {
    return (
      <section aria-label="Loading span" aria-busy="true" className="flex min-h-0 flex-1 flex-col">
        {summary?.span_id === spanId ? <SpanHeader span={summary} detached={detached} /> : null}
        {slowFirstLoad ? (
          <div className="flex flex-col gap-2 p-3">
            <Skeleton className="h-4 w-2/3" />
            <Skeleton className="h-4 w-full" />
            <Skeleton className="h-4 w-1/2" />
          </div>
        ) : null}
      </section>
    );
  }
  if (span.isError) {
    return (
      <p className="p-3 text-sm">{problemOf(span.error)?.title ?? "Could not load this span"}</p>
    );
  }

  const detail = span.data;
  // While the detail is a placeholder the header shows the selected span's summary, never the previous span.
  const headerSpan: HeaderSpan | undefined = placeholder
    ? summary?.span_id === spanId
      ? summary
      : undefined
    : detail;
  const messages = hasMessages(detail);
  const tab = activeTab(choice, messages);
  return (
    <TooltipProvider>
      <section aria-label="Span" className="relative flex min-h-0 flex-1 flex-col">
        {slow ? (
          <div
            role="progressbar"
            aria-label="Loading span"
            className="absolute inset-x-0 top-0 z-10 h-0.5 animate-pulse bg-primary/60"
          />
        ) : null}
        {headerSpan ? <SpanHeader span={headerSpan} detached={detached} /> : null}
        <Tabs
          value={tab}
          onValueChange={select}
          className={cn("min-h-0 flex-1 gap-0 transition-opacity", slow && "opacity-60")}
        >
          <TabsList variant="line" className="h-9 px-3">
            {messages ? (
              <TabsTrigger value="chat" className="text-[12px]">
                Chat
              </TabsTrigger>
            ) : null}
            <TabsTrigger value="io" className="text-[12px]">
              Input / Output
            </TabsTrigger>
            <TabsTrigger value="attributes" className="text-[12px]">
              Attributes
            </TabsTrigger>
            <TabsTrigger value="scores" className="text-[12px]">
              Scores
            </TabsTrigger>
            <TabsTrigger value="annotate" className="text-[12px]">
              Annotate
            </TabsTrigger>
          </TabsList>
          {messages ? (
            <TabsContent value="chat" className="min-h-0 overflow-auto px-4 pb-4 pt-3">
              <ChatTab span={detail} />
            </TabsContent>
          ) : null}
          <TabsContent value="io" className="min-h-0 overflow-auto px-4 pb-4 pt-3">
            <IoTab span={detail} />
          </TabsContent>
          <TabsContent value="attributes" className="min-h-0 overflow-auto px-4 pb-4 pt-3">
            <AttributesTab span={detail} onAddFilter={onAddFilter} />
          </TabsContent>
          <TabsContent value="scores" className="min-h-0 overflow-auto px-4 pb-4 pt-3">
            <ScoresTab traceId={traceId} spanId={spanId} />
          </TabsContent>
          <TabsContent value="annotate" className="min-h-0 overflow-auto px-4 pb-4 pt-3">
            <AnnotateTab traceId={traceId} spanId={spanId} />
          </TabsContent>
        </Tabs>
      </section>
    </TooltipProvider>
  );
}

function hasMessages(span: Schema<"SpanDetail">): boolean {
  return span.input?.format === "messages" || span.output?.format === "messages";
}

function isSpanTab(value: string): value is SpanTab {
  return (
    value === "chat" ||
    value === "io" ||
    value === "attributes" ||
    value === "scores" ||
    value === "annotate"
  );
}

function readStoredTab(): SpanTab | null {
  const stored = localStorage.getItem(TAB_KEY);
  return stored !== null && isSpanTab(stored) ? stored : null;
}

function activeTab(choice: SpanTab | null, messages: boolean): SpanTab {
  if (choice === "scores" || choice === "annotate" || choice === "attributes" || choice === "io") {
    return choice;
  }
  if (choice === "chat" && messages) return "chat";
  if (messages) return "chat";
  return "io";
}
