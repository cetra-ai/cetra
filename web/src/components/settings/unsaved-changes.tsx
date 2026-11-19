import { useBlocker } from "@tanstack/react-router";
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

/**
 * Blocks navigation and closing the tab while `dirty`, and asks before discarding the edits.
 * Cancel and Esc keep editing. With `ignoreSearchChanges`, a change of the URL search alone (the
 * same page with other filters) is not blocked: only leaving the page is.
 */
export function UnsavedChangesGuard({
  dirty,
  ignoreSearchChanges = false,
}: {
  dirty: boolean;
  ignoreSearchChanges?: boolean;
}) {
  const blocker = useBlocker({
    shouldBlockFn: ({ current, next }) =>
      dirty && (!ignoreSearchChanges || current.pathname !== next.pathname),
    enableBeforeUnload: () => dirty,
    withResolver: true,
  });
  const blocked = blocker.status === "blocked";

  return (
    <AlertDialog
      open={blocked}
      onOpenChange={(open) => {
        if (!open && blocked) {
          blocker.reset();
        }
      }}
    >
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>Discard unsaved changes?</AlertDialogTitle>
          <AlertDialogDescription>Your edits are not saved.</AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>Keep editing</AlertDialogCancel>
          <AlertDialogAction
            variant="destructive"
            onClick={(event) => {
              // Keep the dialog open while the navigation it allows runs.
              event.preventDefault();
              if (blocked) {
                blocker.proceed();
              }
            }}
          >
            Discard
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
