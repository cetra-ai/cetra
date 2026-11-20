import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ChevronDown, Pencil, Trash2 } from "lucide-react";
import { useEffect, useState } from "react";
import { $api } from "@/api/client";
import { problemMessage } from "@/api/problem";
import { projectRef } from "@/api/project-ref";
import { conversationKey, conversationsKey, conversationsQuery } from "@/api/queries-assistant";
import type { Schema } from "@/api/types";
import { SubmitButton } from "@/components/form/submit-button";
import { TextField } from "@/components/form/text-field";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { setCurrentConversation, useCurrentConversation } from "@/lib/assistant/panel";
import { formatRelative } from "@/lib/format";

const CONVERSATION_PATH = "/api/v1/projects/{project}/assistant/conversations/{id}";
const TITLE_MAX = 80;

function titleOf(title: string): string {
  return title.trim() === "" ? "Untitled" : title;
}

/**
 * Creates a chat and selects it. The menu and the panel's New chat button share it. `onError`
 * runs when the server refuses the chat, so the caller can drop what waited for it.
 */
export function useNewChat(org: string, project: string, onError: () => void) {
  const queryClient = useQueryClient();
  const ref = projectRef(org, project);
  const create = $api.useMutation("post", "/api/v1/projects/{project}/assistant/conversations", {
    gcTime: 0,
    onError,
  });
  function newChat(): void {
    create.mutate(
      { params: { path: { project: ref } } },
      {
        onSuccess: (conversation) => {
          // Put the chat in the cached list first, so the menu never drops the new selection.
          queryClient.setQueryData<Schema<"AssistantConversationPage">>(
            conversationsKey(org, project),
            (old) => (old ? { ...old, data: [conversation, ...old.data] } : old),
          );
          setCurrentConversation(ref, conversation.id);
          void queryClient.invalidateQueries({ queryKey: conversationsKey(org, project) });
        },
      },
    );
  }
  return { newChat, pending: create.isPending };
}

function RenameDialog({
  org,
  project,
  id,
  initialTitle,
  onDone,
}: {
  org: string;
  project: string;
  id: string;
  initialTitle: string;
  onDone: () => void;
}) {
  const queryClient = useQueryClient();
  const ref = projectRef(org, project);
  const [title, setTitle] = useState(initialTitle);
  const rename = $api.useMutation("patch", CONVERSATION_PATH, { gcTime: 0 });
  const trimmed = title.trim();
  const valid = trimmed.length > 0 && trimmed.length <= TITLE_MAX;
  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) {
          onDone();
        }
      }}
    >
      <DialogContent>
        <form
          onSubmit={(event) => {
            event.preventDefault();
            if (!valid || rename.isPending) {
              return;
            }
            rename.mutate(
              { params: { path: { project: ref, id } }, body: { title: trimmed } },
              {
                onSuccess: () => {
                  void queryClient.invalidateQueries({
                    queryKey: conversationsKey(org, project),
                  });
                  void queryClient.invalidateQueries({
                    queryKey: conversationKey(org, project, id),
                  });
                  onDone();
                },
              },
            );
          }}
          className="flex flex-col gap-4"
        >
          <DialogHeader>
            <DialogTitle>Rename chat</DialogTitle>
          </DialogHeader>
          <TextField
            id="assistant-chat-title"
            label="Title"
            value={title}
            onChange={setTitle}
            maxLength={TITLE_MAX}
            required
            error={
              rename.error
                ? problemMessage(rename.error, "The chat could not be renamed.")
                : undefined
            }
          />
          <DialogFooter>
            <Button type="button" variant="outline" onClick={onDone}>
              Cancel
            </Button>
            <SubmitButton pending={rename.isPending} disabled={!valid}>
              Rename
            </SubmitButton>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function DeleteDialog({
  org,
  project,
  id,
  onDone,
}: {
  org: string;
  project: string;
  id: string;
  onDone: () => void;
}) {
  const queryClient = useQueryClient();
  const ref = projectRef(org, project);
  const remove = $api.useMutation("delete", CONVERSATION_PATH, { gcTime: 0 });
  return (
    <AlertDialog
      open
      onOpenChange={(open) => {
        if (!open) {
          onDone();
        }
      }}
    >
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>Delete this chat?</AlertDialogTitle>
          <AlertDialogDescription>This cannot be undone.</AlertDialogDescription>
        </AlertDialogHeader>
        {remove.error ? (
          <p role="alert" className="text-caption text-destructive">
            {problemMessage(remove.error, "The chat could not be deleted.")}
          </p>
        ) : null}
        <AlertDialogFooter>
          <AlertDialogCancel>Cancel</AlertDialogCancel>
          <AlertDialogAction
            variant="destructive"
            disabled={remove.isPending}
            onClick={(event) => {
              // Keep the dialog open until the server answers.
              event.preventDefault();
              remove.mutate(
                { params: { path: { project: ref, id } } },
                {
                  onSuccess: () => {
                    setCurrentConversation(ref, undefined);
                    void queryClient.invalidateQueries({
                      queryKey: conversationsKey(org, project),
                    });
                    onDone();
                  },
                },
              );
            }}
          >
            Delete
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

/** The chat list in the panel header: the trigger shows the selected chat, the items select one. */
export function ConversationMenu({
  org,
  project,
  onSelect,
}: {
  org: string;
  project: string;
  /** Runs when the user picks a chat from the list, before it is shown. */
  onSelect?: () => void;
}) {
  const ref = projectRef(org, project);
  const list = useQuery(conversationsQuery(org, project));
  const conversations = list.data?.data;
  const current = useCurrentConversation(ref);
  const selected = conversations?.find((conversation) => conversation.id === current);
  const [renaming, setRenaming] = useState(false);
  const [deleting, setDeleting] = useState(false);

  // A chat that is no longer listed (deleted elsewhere, or past the newest 100) is not selected.
  useEffect(() => {
    if (
      conversations !== undefined &&
      current !== undefined &&
      !conversations.some((conversation) => conversation.id === current)
    ) {
      setCurrentConversation(ref, undefined);
    }
  }, [conversations, current, ref]);

  const label = selected && selected.title.trim() !== "" ? selected.title : "Untitled chat";
  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="min-w-0 max-w-56 justify-start"
          >
            <span className="truncate">{label}</span>
            <ChevronDown aria-hidden="true" />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start" className="w-72">
          <DropdownMenuLabel>Chats</DropdownMenuLabel>
          {conversations !== undefined && conversations.length === 0 ? (
            <DropdownMenuItem disabled>No chats yet</DropdownMenuItem>
          ) : null}
          {conversations?.map((conversation) => (
            <DropdownMenuItem
              key={conversation.id}
              onSelect={() => {
                onSelect?.();
                setCurrentConversation(ref, conversation.id);
              }}
              className="flex flex-col items-start gap-0"
            >
              <span className="w-full truncate">{titleOf(conversation.title)}</span>
              <span className="text-caption text-muted-foreground">
                {formatRelative(conversation.last_active_at, Date.now())}
              </span>
            </DropdownMenuItem>
          ))}
          {current !== undefined ? (
            <>
              <DropdownMenuSeparator />
              <DropdownMenuItem
                onSelect={() => {
                  setRenaming(true);
                }}
              >
                <Pencil />
                Rename
              </DropdownMenuItem>
              <DropdownMenuItem
                className="text-destructive"
                onSelect={() => {
                  setDeleting(true);
                }}
              >
                <Trash2 />
                Delete
              </DropdownMenuItem>
            </>
          ) : null}
        </DropdownMenuContent>
      </DropdownMenu>
      {renaming && current !== undefined ? (
        <RenameDialog
          org={org}
          project={project}
          id={current}
          initialTitle={selected?.title ?? ""}
          onDone={() => {
            setRenaming(false);
          }}
        />
      ) : null}
      {deleting && current !== undefined ? (
        <DeleteDialog
          org={org}
          project={project}
          id={current}
          onDone={() => {
            setDeleting(false);
          }}
        />
      ) : null}
    </>
  );
}
