import { useState } from "react";
import type { Schema } from "@/api/types";
import { CopyButton } from "@/components/copy-button";
import { FormatSwitch, usePayloadFormat } from "@/components/payload/format-switch";
import { JsonView } from "@/components/payload/json-view";
import { Markdown } from "@/components/payload/markdown";
import { MessageView } from "@/components/payload/message-view";
import { ShowMore } from "@/components/payload/show-more";
import { Badge } from "@/components/ui/badge";
import { foldedMessage, toolLinks } from "@/lib/tool-calls";

/** Pretty rendering stops here until the user asks for it. Serialized characters, not bytes. */
const RENDER_CAP = 1_000_000;

export function PayloadView({
  payload,
  label,
}: {
  payload: Schema<"Payload"> | undefined;
  label: string;
}) {
  const [mode, setMode] = usePayloadFormat();
  const [force, setForce] = useState(false);
  const [seen, setSeen] = useState(payload);
  if (seen !== payload) {
    setSeen(payload);
    setForce(false);
  }
  if (!payload) {
    return (
      <section className="flex min-w-0 flex-col gap-1">
        <h3 className="text-body font-medium">{label}</h3>
        <p className="text-body text-muted-foreground">—</p>
      </section>
    );
  }

  const serialized = serializedPayload(payload);
  const capped = serialized.chars > RENDER_CAP && mode === "pretty" && !force;
  return (
    <section className="flex min-w-0 flex-col gap-1">
      <div className="flex flex-wrap items-center gap-2">
        <h3 className="text-body font-medium">{label}</h3>
        <Badge variant="outline" className="h-4 px-1.5 text-caption text-muted-foreground">
          {payload.format}
        </Badge>
        <span className="text-caption text-muted-foreground">{serialized.chars} chars</span>
        <CopyButton value={serialized.raw} label={label} />
        <FormatSwitch value={mode} onChange={setMode} />
      </div>
      {capped ? (
        <div className="flex flex-wrap items-center gap-2 text-body">
          <p>Large payload ({megabytes(serialized.chars)} MB)</p>
          <button type="button" className="underline" onClick={() => setForce(true)}>
            Render anyway
          </button>
        </div>
      ) : payload.format === "messages" && mode === "pretty" ? (
        // Messages bring their own cards, one per role.
        <PayloadBody payload={payload} mode={mode} serialized={serialized} />
      ) : (
        // Text and JSON sit in the same card as a message, without a role header.
        <div className="rounded-md border border-l-2 border-l-muted-foreground/40 bg-card px-3 py-2">
          <PayloadBody payload={payload} mode={mode} serialized={serialized} />
        </div>
      )}
    </section>
  );
}

function PayloadBody({
  payload,
  mode,
  serialized,
}: {
  payload: Schema<"Payload">;
  mode: "pretty" | "json" | "raw";
  serialized: Serialized;
}) {
  if (payload.format === "text") {
    // Only Pretty renders Markdown. JSON and Raw both show the plain text.
    if (mode === "pretty") {
      return (
        <ShowMore>
          <Markdown text={payload.value} />
        </ShowMore>
      );
    }
    return (
      <ShowMore>
        <pre className="overflow-auto font-mono text-body whitespace-pre-wrap">{payload.value}</pre>
      </ShowMore>
    );
  }
  if (payload.format === "messages" && mode === "pretty") {
    return <MessageList messages={payload.value} />;
  }
  if (mode === "raw") {
    return (
      <ShowMore>
        <pre className="overflow-auto font-mono text-body whitespace-pre-wrap">
          {serialized.raw}
        </pre>
      </ShowMore>
    );
  }
  if (payload.format === "json" && mode === "json") {
    return (
      <ShowMore>
        <pre className="overflow-auto font-mono text-body whitespace-pre-wrap">
          {serialized.pretty}
        </pre>
      </ShowMore>
    );
  }
  return (
    <ShowMore>
      <JsonView value={payload.value} />
    </ShowMore>
  );
}

/** Each message is the same card the Chat tab shows, so Input/Output and Chat look alike. */
function MessageList({ messages }: { messages: readonly Schema<"Message">[] }) {
  const links = toolLinks(messages);
  const seen = new Map<string, number>();
  const shown = messages.filter((message) => !foldedMessage(message, links));
  return (
    <div className="flex flex-col gap-2">
      {shown.map((message) => {
        const base = messageKey(message);
        const n = seen.get(base) ?? 0;
        seen.set(base, n + 1);
        return <MessageView key={`${base}\0${n}`} message={message} links={links} />;
      })}
    </div>
  );
}

function messageKey(message: Schema<"Message">): string {
  const name = message.name ?? "";
  return `${message.role}\0${name}\0${messageText(message)}`;
}

function messageText(message: Schema<"Message">): string {
  const lines: string[] = [];
  for (const part of message.parts) {
    if (part.type === "text" && typeof part.content === "string") {
      lines.push(part.content);
    }
  }
  return lines.join("\n");
}

type Serialized = { pretty: string; raw: string; chars: number };

function serializedPayload(payload: Schema<"Payload">): Serialized {
  if (payload.format === "text") {
    return { pretty: payload.value, raw: payload.value, chars: payload.value.length };
  }
  const raw = JSON.stringify(payload.value) ?? "";
  const pretty = JSON.stringify(payload.value, null, 2) ?? "";
  return { pretty, raw, chars: raw.length };
}

function megabytes(chars: number): string {
  const mb = chars / 1_000_000;
  return mb >= 10 ? mb.toFixed(0) : mb.toFixed(1);
}
