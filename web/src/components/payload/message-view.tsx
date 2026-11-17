import { cn } from "cn";
import { Bot, MessageSquare, Settings2, User, Wrench } from "lucide-react";
import type { ReactNode } from "react";
import type { Schema } from "@/api/types";
import { PartView, type ToolLinks } from "@/components/payload/part-view";
import { ShowMore } from "@/components/payload/show-more";
import { Badge } from "@/components/ui/badge";

/** One chat message as a card: a role header, then its parts. Long messages are clipped. */
export function MessageView({
  message,
  projectRef,
  links,
}: {
  message: Schema<"Message">;
  projectRef?: string;
  links: ToolLinks;
}) {
  const role = message.role.length > 0 ? message.role : "other";
  const Icon = roleIcon(role);
  return (
    <article className={cn("rounded-md border border-l-2 bg-card", roleBorder(role))}>
      <header className="flex flex-wrap items-center gap-2 border-b px-3 py-1.5 text-[12px]">
        <Icon className="size-3.5 text-muted-foreground" aria-hidden />
        <span className="font-medium">{capitalise(role)}</span>
        {message.name ? <span className="text-muted-foreground">{message.name}</span> : null}
        {message.finish_reason ? (
          <Badge variant="outline" className="text-[11px] text-muted-foreground">
            {message.finish_reason}
          </Badge>
        ) : null}
      </header>
      <div className="px-3 py-2">
        <ShowMore>
          <div className="flex flex-col gap-1">{partsOf(message, projectRef, links)}</div>
        </ShowMore>
      </div>
    </article>
  );
}

function partsOf(
  message: Schema<"Message">,
  projectRef: string | undefined,
  links: ToolLinks,
): ReactNode[] {
  const seen = new Map<string, number>();
  return message.parts.map((part) => (
    <PartView key={partKey(part, seen)} part={part} projectRef={projectRef} links={links} />
  ));
}

function partKey(part: Schema<"Part">, seen: Map<string, number>): string {
  const id = "id" in part && typeof part.id === "string" ? part.id : "";
  const content =
    "content" in part && typeof part.content === "string" ? part.content.slice(0, 24) : "";
  const base = `${part.type ?? "part"}\0${id}\0${content}`;
  const n = seen.get(base) ?? 0;
  seen.set(base, n + 1);
  return `${base}\0${n}`;
}

function capitalise(text: string): string {
  return text.charAt(0).toUpperCase() + text.slice(1);
}

function roleIcon(role: string) {
  if (role === "user") return User;
  if (role === "assistant") return Bot;
  if (role === "system") return Settings2;
  if (role === "tool") return Wrench;
  return MessageSquare;
}

function roleBorder(role: string): string {
  if (role === "system") return "border-l-muted-foreground";
  if (role === "user") return "border-l-kind-span";
  if (role === "assistant") return "border-l-kind-llm";
  if (role === "tool") return "border-l-kind-tool";
  return "border-l-kind-agent";
}
