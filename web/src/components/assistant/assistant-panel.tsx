import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useRouterState } from "@tanstack/react-router";
import { SquarePen, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { fieldErrors, problemOf } from "@/api/problem";
import {
  assistantStatusQuery,
  conversationKey,
  conversationQuery,
  conversationsKey,
  turnUrl,
} from "@/api/queries-assistant";
import { Composer } from "@/components/assistant/composer";
import { ConversationMenu, useNewChat } from "@/components/assistant/conversation-menu";
import { Transcript } from "@/components/assistant/transcript";
import { Unavailable } from "@/components/assistant/unavailable";
import { useProject } from "@/components/shell/project-context";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { TooltipProvider } from "@/components/ui/tooltip";
import { fitContext, type PageContext, pageContext, withoutField } from "@/lib/assistant/context";
import { setPanelOpen, useCurrentConversation } from "@/lib/assistant/panel";
import {
  type AssistantBubble,
  applyEvent,
  type Bubble,
  bubblesOf,
} from "@/lib/assistant/transcript";
import { parseTurnEvent, type TurnEvent } from "@/lib/assistant/types";
import { postEventStream, StreamRequestError } from "@/lib/event-stream";
import { LLM_ADAPTER_LABELS } from "@/lib/llm-adapters";
import { activeTabOf, readTabs } from "@/lib/sql/tabs";

const PAUSE_HINT = "Ask an admin to check the LLM connection.";
const RESTORE_TEXT = "The assistant is paused while this server is in restore mode.";
const LOST_TEXT = "The connection to the server was lost.";
const ASK_TEXT = "The question could not be sent.";
const EMPTY_TEXT = "Ask about this project's traces, metrics, evaluations and settings.";
const CREATE_TEXT = "The chat could not be created.";

/** A message the panel shows under the transcript. `retry` resends the same message and context. */
type Notice = { text: string; hint?: string; retry?: { message: string; context: PageContext } };

/** The turn on screen: its user and assistant bubbles, owned by the request that streams them. */
type Live = { owner: AbortController; conversationId: string; bubbles: Bubble[] };

function isAbortError(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    (error as { name?: unknown }).name === "AbortError"
  );
}

/** The text for a turn that was refused before it started (the question is not stored). */
function requestText(error: unknown): string {
  if (!(error instanceof StreamRequestError)) {
    return ASK_TEXT;
  }
  const detail = problemOf(error.problem)?.detail;
  switch (error.status) {
    case 0:
      return LOST_TEXT;
    case 400:
      return fieldErrors(error.problem).message ?? detail ?? ASK_TEXT;
    case 409:
      return detail ?? "A turn is already running in this conversation.";
    case 429:
      return error.retryAfter !== undefined
        ? `Too many questions. Try again in ${error.retryAfter} seconds.`
        : "Too many questions. Try again in a minute.";
    case 503:
      return RESTORE_TEXT;
    default:
      return detail ?? ASK_TEXT;
  }
}

function errorNotice(
  error: Extract<TurnEvent, { event: "error" }>["data"],
  message: string,
  context: PageContext,
): Notice {
  switch (error.class) {
    case "retryable":
      return { text: error.message, retry: { message, context } };
    case "pause":
      return { text: error.message, hint: PAUSE_HINT };
    case "permanent":
      return { text: error.message };
  }
}

/** Applies `change` to the streaming (last) assistant bubble of `previous`, when it is still `owner`'s. */
function streamingUpdate(
  previous: Live | undefined,
  owner: AbortController,
  change: (bubble: AssistantBubble) => AssistantBubble,
): Live | undefined {
  if (previous === undefined || previous.owner !== owner) {
    return previous;
  }
  return {
    ...previous,
    bubbles: previous.bubbles.map((bubble) =>
      bubble.role === "assistant" ? change(bubble) : bubble,
    ),
  };
}

/**
 * The assistant panel: the chat list, the transcript of the selected chat, the composer and the
 * connection notice. Mounted inside the project layout and keyed by project by its caller.
 */
export function AssistantPanel() {
  const { org, project, ref } = useProject();
  const queryClient = useQueryClient();
  const status = useQuery(assistantStatusQuery(org, project));
  const usable = status.data?.usable === true;
  const current = useCurrentConversation(ref);
  const detail = useQuery({
    ...conversationQuery(org, project, current ?? ""),
    enabled: usable && current !== undefined,
  });
  const location = useRouterState({ select: (state) => state.location });

  const [draft, setDraft] = useState("");
  const [streaming, setStreaming] = useState(false);
  const [live, setLive] = useState<Live | undefined>(undefined);
  const [notice, setNotice] = useState<Notice | undefined>(undefined);
  const [removed, setRemoved] = useState<{ page: string; keys: string[] }>({ page: "", keys: [] });
  const turnRef = useRef<{ conversationId: string; controller: AbortController } | undefined>(
    undefined,
  );
  // A question waiting for its chat to be created (a send with no chat selected).
  const pendingRef = useRef<{ message: string; context: PageContext } | undefined>(undefined);
  // A refused chat drops the question waiting for it: it is not sent to a chat picked later.
  const { newChat, pending } = useNewChat(org, project, () => {
    pendingRef.current = undefined;
    setNotice({ text: CREATE_TEXT });
  });

  // The page context now: read at call time, so SQL typed since the last render is included.
  function contextNow(): PageContext {
    const tab = activeTabOf(readTabs(ref));
    const full = pageContext({
      org,
      project,
      pathname: location.pathname,
      search: location.search,
      sql: { query: tab.draft.query, params: tab.draft.params },
    });
    const keys = removed.page === full.page ? removed.keys : [];
    return keys.reduce((context, key) => withoutField(context, key), full);
  }
  const shown = contextNow();

  const startTurn = useCallback(
    (conversationId: string, message: string, context: PageContext) => {
      const owner = new AbortController();
      turnRef.current = { conversationId, controller: owner };
      setDraft("");
      setNotice(undefined);
      setStreaming(true);
      setLive({
        owner,
        conversationId,
        bubbles: [
          { role: "user", seq: 0, text: message, context },
          { role: "assistant", seqs: [], blocks: [], status: "streaming" },
        ],
      });
      void (async () => {
        try {
          await postEventStream(
            turnUrl(org, project, conversationId),
            { message, context },
            {
              signal: owner.signal,
              onBlock: (block) => {
                const event = parseTurnEvent(block.event, block.data);
                if (event === null) {
                  return;
                }
                setLive((previous) =>
                  streamingUpdate(previous, owner, (bubble) => applyEvent(bubble, event)),
                );
                if (event.event === "error") {
                  setNotice(errorNotice(event.data, message, context));
                }
              },
            },
          );
        } catch (error) {
          if (isAbortError(error)) {
            setLive((previous) =>
              streamingUpdate(previous, owner, (bubble) => ({ ...bubble, status: "cancelled" })),
            );
          } else {
            // Refused before the stream: the question was not stored, so it goes back in the box.
            setLive((previous) => (previous?.owner === owner ? undefined : previous));
            setDraft((previous) => (previous === "" ? message : previous));
            setNotice({ text: requestText(error) });
          }
        } finally {
          if (turnRef.current?.controller === owner) {
            turnRef.current = undefined;
            setStreaming(false);
          }
        }
        // The stored messages replace the live bubbles.
        await Promise.all([
          queryClient.invalidateQueries({
            queryKey: conversationKey(org, project, conversationId),
          }),
          queryClient.invalidateQueries({ queryKey: conversationsKey(org, project) }),
        ]);
        setLive((previous) => (previous?.owner === owner ? undefined : previous));
      })();
    },
    [org, project, queryClient],
  );

  // Switching chats aborts a turn that belongs to the chat left, and drops the notice.
  useEffect(() => {
    const turn = turnRef.current;
    if (turn !== undefined && turn.conversationId !== current) {
      turn.controller.abort();
    }
    setNotice(undefined);
  }, [current]);

  // A question sent with no chat selected starts once the new chat is selected.
  useEffect(() => {
    const pending = pendingRef.current;
    if (pending !== undefined && current !== undefined) {
      pendingRef.current = undefined;
      startTurn(current, pending.message, pending.context);
    }
  }, [current, startTurn]);

  // Closing the panel or leaving the project unmounts it and aborts a running turn.
  useEffect(() => {
    return () => {
      turnRef.current?.controller.abort();
    };
  }, []);

  function send() {
    const message = draft.trim();
    if (message === "") {
      return;
    }
    const context = fitContext(contextNow());
    if (current === undefined) {
      pendingRef.current = { message, context };
      newChat();
      return;
    }
    startTurn(current, message, context);
  }

  const stored = detail.data !== undefined ? bubblesOf(detail.data.messages) : [];
  const bubbles: Bubble[] =
    live !== undefined && live.conversationId === current ? [...stored, ...live.bubbles] : stored;

  return (
    <TooltipProvider>
      <section
        aria-label="Assistant"
        className="flex h-full min-h-0 flex-col bg-background text-body"
        onKeyDown={(event) => {
          // Esc leaves the panel's controls for the page. It does not close the panel (D-AS-27).
          if (event.key === "Escape") {
            (document.activeElement as HTMLElement | null)?.blur();
            event.stopPropagation();
          }
        }}
      >
        <header className="flex h-10 shrink-0 items-center gap-1 border-b px-2">
          <ConversationMenu
            org={org}
            project={project}
            onSelect={() => {
              pendingRef.current = undefined;
            }}
          />
          <div className="flex-1" />
          <Button
            type="button"
            variant="ghost"
            size="icon-sm"
            aria-label="New chat"
            disabled={pending}
            onClick={newChat}
          >
            <SquarePen />
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="icon-sm"
            aria-label="Close the assistant"
            onClick={() => {
              setPanelOpen(false);
            }}
          >
            <X />
          </Button>
        </header>
        {status.isPending ? (
          <div className="flex flex-col gap-2 p-4">
            <Skeleton className="h-4 w-3/4" />
            <Skeleton className="h-4 w-1/2" />
          </div>
        ) : usable ? (
          <>
            <Transcript
              org={org}
              project={project}
              conversationId={current ?? ""}
              bubbles={bubbles}
              empty={EMPTY_TEXT}
            />
            {notice !== undefined ? (
              <div
                role="alert"
                className="flex shrink-0 flex-col gap-1 border-t px-3 py-2 text-caption"
              >
                <p className="text-destructive">{notice.text}</p>
                {notice.hint !== undefined ? (
                  <p className="text-muted-foreground">{notice.hint}</p>
                ) : null}
                {notice.retry !== undefined && current !== undefined ? (
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    className="w-fit"
                    onClick={() => {
                      if (notice.retry !== undefined) {
                        startTurn(current, notice.retry.message, notice.retry.context);
                      }
                    }}
                  >
                    Retry
                  </Button>
                ) : null}
              </div>
            ) : null}
            <Composer
              value={draft}
              onChange={setDraft}
              onSend={send}
              onStop={() => {
                turnRef.current?.controller.abort();
              }}
              streaming={streaming}
              context={shown}
              onRemove={(key) => {
                setRemoved({
                  page: shown.page,
                  keys: [...(removed.page === shown.page ? removed.keys : []), key],
                });
              }}
            />
          </>
        ) : (
          <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
            <Unavailable reason={status.data?.reason} />
          </div>
        )}
        {usable && status.data ? (
          <p className="shrink-0 border-t p-3 text-caption text-muted-foreground">
            Answers use{" "}
            <span className="font-medium text-foreground">
              {status.data.connection_name ?? "an LLM connection"}
            </span>{" "}
            (
            {[
              status.data.adapter ? adapterLabel(status.data.adapter) : undefined,
              status.data.model,
            ]
              .filter((part) => part !== undefined && part !== "")
              .join(", ")}
            ). What the assistant reads is sent to that provider.
          </p>
        ) : null}
      </section>
    </TooltipProvider>
  );
}

/** The adapter's display name; the status route returns the adapter as a plain string. */
function adapterLabel(adapter: string): string {
  return adapter in LLM_ADAPTER_LABELS
    ? LLM_ADAPTER_LABELS[adapter as keyof typeof LLM_ADAPTER_LABELS]
    : adapter;
}
