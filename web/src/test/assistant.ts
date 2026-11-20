import { act, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";

/**
 * Types into the assistant's message box. The box is focused directly, not clicked: jsdom has no
 * layout (every rect is zero), so a pointer press anywhere counts as a press on the panel
 * group's resize handle, which takes the focus.
 */
export async function typeMessage(user: UserEvent, region: HTMLElement, text: string) {
  const box = within(region).getByRole("textbox", { name: "Message the assistant" });
  act(() => {
    box.focus();
  });
  await user.keyboard(text);
}

/** Opens a menu trigger from the keyboard, for the same reason `typeMessage` focuses the box. */
export async function openMenu(user: UserEvent, trigger: HTMLElement) {
  act(() => {
    trigger.focus();
  });
  await user.keyboard("{Enter}");
}
