import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { ThumbsDown, ThumbsUp } from "lucide-react";
import { type ReactNode, type UIEvent, useEffect, useRef } from "react";
import { $api } from "@/api/client";
import { projectRef } from "@/api/project-ref";
import { conversationKey } from "@/api/queries-assistant";
import { AnswerMarkdown } from "@/components/assistant/answer-markdown";
import { chipValue } from "@/components/assistant/composer";
import { DraftCard } from "@/components/assistant/draft-card";
import { ToolStep } from "@/components/assistant/tool-step";
import { Button } from "@/components/ui/button";
import type { AssistantBubble, Bubble } from "@/lib/assistant/transcript";

const FEEDBACK_PATH =
  "/api/v1/projects/{project}/assistant/conversations/{id}/messages/{seq}/feedback";
/** Within this many pixels of the bottom, new content keeps the view at the bottom. */
const STICK_PX = 48;

type Rate = (seq: number, feedback: "up" | "down" | null) => void;

function contextEntries(
  context: Record<string, unknown> | undefined,
): [string, string | number | boolean][] {
  return Object.entries(context ?? {}).filter(
    (entry): entry is [string, string | number | boolean] =>
      typeof entry[1] === "string" || typeof entry[1] === "number" || typeof entry[1] === "boolean",
  );
}

function UserBubble({ text, context }: { text: string; context?: Record<string, unknown> }) {
  const chips = contextEntries(context);
  return (
    <div className="flex flex-col items-end gap-1.5">
      <div className="max-w-[85%] rounded-md bg-muted p-2.5 text-ui break-words whitespace-pre-wrap">
        {text}
      </div>
      {chips.length > 0 ? (
        <ul aria-label="Context of this question" className="flex flex-wrap justify-end gap-1">
          {chips.map(([key, value]) => (
            <li
              key={key}
              className="rounded-sm border px-1.5 py-0.5 text-caption text-muted-foreground"
            >
              {key}: {chipValue(value)}
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

function AssistantBubbleView({
  org,
  project,
  bubble,
  rate,
}: {
  org: string;
  project: string;
  bubble: AssistantBubble;
  rate: Rate;
}) {
  const feedbackSeq = bubble.feedbackSeq;
  return (
    <div className="flex flex-col gap-2 text-ui">
      {bubble.blocks.map((block, index) => {
        if (block.type === "text") {
          // biome-ignore lint/suspicious/noArrayIndexKey: blocks only append, so a text block keeps its index
          return <AnswerMarkdown key={`text-${index}`} text={block.text} />;
        }
        if (block.type === "step") {
          return <ToolStep key={`step-${block.step.id}`} step={block.step} />;
        }
        return <DraftCard key={`draft-${block.id}`} kind={block.kind} draft={block.draft} />;
      })}
      {bubble.status === "streaming" && bubble.blocks.length === 0 ? (
        <p className="text-muted-foreground">Thinking…</p>
      ) : null}
      {bubble.status === "cancelled" ? (
        <p className="text-caption text-muted-foreground">Stopped</p>
      ) : null}
      {bubble.status === "failed" ? (
        <p className="text-caption text-destructive">This answer failed.</p>
      ) : null}
      {bubble.status === "complete" && feedbackSeq !== undefined ? (
        <div className="flex items-center gap-1">
          <Button
            type="button"
            variant="ghost"
            size="icon-sm"
            aria-label="Thumbs up"
            aria-pressed={bubble.feedback === "up"}
            onClick={() => {
              rate(feedbackSeq, bubble.feedback === "up" ? null : "up");
            }}
          >
            <ThumbsUp />
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="icon-sm"
            aria-label="Thumbs down"
            aria-pressed={bubble.feedback === "down"}
            onClick={() => {
              rate(feedbackSeq, bubble.feedback === "down" ? null : "down");
            }}
          >
            <ThumbsDown />
          </Button>
        </div>
      ) : null}
      {bubble.traceId !== undefined ? (
        <Link
          to="/$org/$project/traces/$traceId"
          params={{ org, project, traceId: bubble.traceId }}
          className="w-fit text-caption text-primary underline underline-offset-2"
        >
          View trace
        </Link>
      ) : null}
    </div>
  );
}

/**
 * The bubbles of one conversation. It scrolls to the bottom on new content, unless the reader has
 * scrolled up. Thumbs rate a complete answer (`PUT …/messages/{seq}/feedback`).
 */
export function Transcript({
  org,
  project,
  conversationId,
  bubbles,
  empty,
}: {
  org: string;
  project: string;
  conversationId: string;
  bubbles: Bubble[];
  /** Shown while the chat has no messages. */
  empty: ReactNode;
}) {
  const ref = projectRef(org, project);
  const queryClient = useQueryClient();
  const feedback = $api.useMutation("put", FEEDBACK_PATH, { gcTime: 0 });
  const scroller = useRef<HTMLDivElement>(null);
  const stick = useRef(true);

  // Another chat starts at its newest content, even when the reader had scrolled up in the last one.
  // biome-ignore lint/correctness/useExhaustiveDependencies: the chat switch is the trigger
  useEffect(() => {
    stick.current = true;
  }, [conversationId]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: new content is the trigger to scroll
  useEffect(() => {
    const element = scroller.current;
    if (element !== null && stick.current) {
      element.scrollTop = element.scrollHeight;
    }
  }, [bubbles]);

  const rate: Rate = (seq, value) => {
    feedback.mutate(
      { params: { path: { project: ref, id: conversationId, seq } }, body: { feedback: value } },
      {
        onSuccess: () => {
          void queryClient.invalidateQueries({
            queryKey: conversationKey(org, project, conversationId),
          });
        },
      },
    );
  };

  function onScroll(event: UIEvent<HTMLDivElement>) {
    const element = event.currentTarget;
    stick.current = element.scrollHeight - element.scrollTop - element.clientHeight < STICK_PX;
  }

  return (
    <div
      ref={scroller}
      onScroll={onScroll}
      className="flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto p-3"
    >
      {bubbles.length === 0 ? (
        <div className="m-auto w-full">{empty}</div>
      ) : (
        bubbles.map((bubble, index) =>
          bubble.role === "user" ? (
            // biome-ignore lint/suspicious/noArrayIndexKey: bubbles only append; a refetch rebuilds the list
            <UserBubble key={index} text={bubble.text} context={bubble.context} />
          ) : (
            <AssistantBubbleView
              // biome-ignore lint/suspicious/noArrayIndexKey: bubbles only append; a refetch rebuilds the list
              key={index}
              org={org}
              project={project}
              bubble={bubble}
              rate={rate}
            />
          ),
        )
      )}
    </div>
  );
}
