export const GO_TARGETS = {
  d: { id: "go-dashboard", to: "/$org/$project/dashboard" },
  t: { id: "go-traces", to: "/$org/$project/traces" },
  p: { id: "go-spans", to: "/$org/$project/spans" },
  s: { id: "go-sessions", to: "/$org/$project/sessions" },
  u: { id: "go-users", to: "/$org/$project/users" },
  a: { id: "go-alerts", to: "/$org/$project/alerts" },
  q: { id: "go-sql", to: "/$org/$project/sql" },
  e: { id: "go-evaluators", to: "/$org/$project/evaluators" },
  o: { id: "go-online-rules", to: "/$org/$project/online-rules" },
  y: { id: "go-issues", to: "/$org/$project/issues" },
  n: { id: "go-issue-sources", to: "/$org/$project/issue-sources" },
  r: { id: "go-annotation-queues", to: "/$org/$project/annotation-queues" },
  i: { id: "go-datasets", to: "/$org/$project/datasets" },
  x: { id: "go-experiments", to: "/$org/$project/experiments" },
  m: { id: "go-prompts", to: "/$org/$project/prompts" },
  l: { id: "go-playground", to: "/$org/$project/playground" },
} as const;

type GoTarget = (typeof GO_TARGETS)[keyof typeof GO_TARGETS];

export type ShortcutId =
  | "focus-search"
  | "filter-text"
  | "row-next"
  | "row-prev"
  | "open"
  | "dismiss"
  | "toggle-mode"
  | "fit"
  | "palette"
  | "help"
  | GoTarget["id"]
  | "assistant";

type Binding = { id: ShortcutId; key: string; mod?: boolean };

export type Shortcut = {
  keys: string;
  description: string;
  scope: string;
  bindings: readonly Binding[];
};

/** One row per shortcut. The help dialog and the key listener both read this. */
export const SHORTCUTS: readonly Shortcut[] = [
  {
    keys: "/",
    description: "Focus search",
    scope: "lists",
    bindings: [{ id: "focus-search", key: "/" }],
  },
  {
    keys: "f",
    description: "Edit the filter as text",
    scope: "lists",
    bindings: [{ id: "filter-text", key: "f" }],
  },
  {
    keys: "j / k, ↓ / ↑",
    description: "Next / previous row",
    scope: "lists, tree, session turns",
    bindings: [
      { id: "row-next", key: "j" },
      { id: "row-next", key: "ArrowDown" },
      { id: "row-prev", key: "k" },
      { id: "row-prev", key: "ArrowUp" },
    ],
  },
  {
    keys: "Enter",
    description: "Open",
    scope: "lists",
    bindings: [{ id: "open", key: "Enter" }],
  },
  {
    keys: "Esc",
    description: "Close the panel, clear focus",
    scope: "anywhere",
    bindings: [{ id: "dismiss", key: "Escape" }],
  },
  {
    keys: "t",
    description: "Cycle Tree / Timeline / Transcript",
    scope: "trace view",
    bindings: [{ id: "toggle-mode", key: "t" }],
  },
  {
    keys: "0",
    description: "Fit the timeline",
    scope: "trace view",
    bindings: [{ id: "fit", key: "0" }],
  },
  {
    keys: "⌘↵",
    description: "Run the query",
    scope: "SQL console",
    bindings: [],
  },
  {
    keys: "⌘↵",
    description: "Complete and go to the next item",
    scope: "Annotation queue",
    bindings: [],
  },
  {
    keys: "⌘⇧↵",
    description: "Next item without completing",
    scope: "Annotation queue",
    bindings: [],
  },
  {
    keys: "g then a letter",
    description: "Go to a project page (the letters are in each sidebar tooltip)",
    scope: "project",
    bindings: [],
  },
  {
    keys: "⌘K",
    description: "Command palette",
    scope: "anywhere",
    bindings: [{ id: "palette", key: "k", mod: true }],
  },
  {
    keys: "⌘I",
    description: "Open or close the assistant",
    scope: "project",
    bindings: [{ id: "assistant", key: "i", mod: true }],
  },
  {
    keys: "?",
    description: "Shortcuts help",
    scope: "anywhere",
    bindings: [{ id: "help", key: "?" }],
  },
];

const ELEMENT_SHORTCUTS: ReadonlySet<ShortcutId> = new Set(["row-next", "row-prev", "open", "fit"]);

export type Chord = "g" | null;

export type KeyInfo = {
  key: string;
  metaKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey?: boolean;
};

export function isElementShortcut(id: ShortcutId): boolean {
  return ELEMENT_SHORTCUTS.has(id);
}

export function goDestination(id: ShortcutId): GoTarget["to"] | undefined {
  for (const target of Object.values(GO_TARGETS)) {
    if (target.id === id) {
      return target.to;
    }
  }
  return undefined;
}

/** The letter that goes to the page `to` (after `g`), used for sidebar tooltips. */
export function goKeyOf(to: string): string | undefined {
  for (const [key, target] of Object.entries(GO_TARGETS)) {
    if (target.to === to) {
      return key;
    }
  }
  return undefined;
}

/** 16 or 32 hex digits, any case, dashes ignored. */
export function hexId(text: string, length: 16 | 32): string | undefined {
  const stripped = text.trim().replaceAll("-", "");
  if (stripped.length !== length || !/^[0-9a-fA-F]+$/.test(stripped)) {
    return undefined;
  }
  return stripped.toLowerCase();
}

export function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) {
    return false;
  }
  if (target.isContentEditable) {
    return true;
  }
  return target.closest("input, textarea, select, [contenteditable], .cm-editor") !== null;
}

export function reduceShortcut(
  chord: Chord,
  key: KeyInfo,
  typing: boolean,
): { chord: Chord; action: ShortcutId | null } {
  const palette = key.key.toLowerCase() === "k" && (key.metaKey || key.ctrlKey) && !key.altKey;
  if (palette) {
    return { chord: null, action: "palette" };
  }
  // Before the typing check: the assistant's composer is a textarea, and Ctrl/Cmd+I must still work there.
  const assistant =
    key.key.toLowerCase() === "i" && (key.metaKey || key.ctrlKey) && !key.altKey && !key.shiftKey;
  if (assistant) {
    return { chord: null, action: "assistant" };
  }
  if (typing || key.metaKey || key.ctrlKey || key.altKey) {
    return { chord: null, action: null };
  }
  if (chord === "g") {
    const go = GO_TARGETS[key.key as keyof typeof GO_TARGETS];
    if (go) {
      return { chord: null, action: go.id };
    }
  }
  if (key.key === "g") {
    return { chord: "g", action: null };
  }
  for (const row of SHORTCUTS) {
    for (const binding of row.bindings) {
      if (!binding.mod && binding.key === key.key) {
        return { chord: null, action: binding.id };
      }
    }
  }
  return { chord: null, action: null };
}

export function matchesBinding(event: KeyInfo, id: ShortcutId): boolean {
  return reduceShortcut(null, event, false).action === id;
}

type ShortcutHandler = (event: KeyboardEvent) => void;

const handlers = new Map<ShortcutId, Set<ShortcutHandler>>();

export function registerShortcut(id: ShortcutId, handler: ShortcutHandler): () => void {
  let set = handlers.get(id);
  if (!set) {
    set = new Set();
    handlers.set(id, set);
  }
  set.add(handler);
  return () => {
    set.delete(handler);
  };
}

export function dispatchShortcut(id: ShortcutId, event: KeyboardEvent): void {
  const set = handlers.get(id);
  if (!set) {
    return;
  }
  for (const handler of [...set]) {
    handler(event);
  }
}

/** Whether the browser runs on an Apple platform, where the shortcuts' modifier is ⌘. */
export function isApplePlatform(userAgent: string): boolean {
  return /Mac|iPhone|iPad/.test(userAgent);
}

/** How the other platforms spell the Apple key symbols. */
const OTHER_PLATFORM_KEYS: Record<string, string> = {
  "⌘": "Ctrl",
  "⇧": "Shift",
  "⌥": "Alt",
  "↵": "Enter",
};

/**
 * A shortcut as the user's platform spells it. `keys` is written the Apple way (`⌘⇧↵`); elsewhere
 * a chord becomes `Ctrl+Shift+Enter`. Keys without a modifier (`g then a letter`) are unchanged.
 */
export function shortcutLabel(keys: string, userAgent: string): string {
  if (isApplePlatform(userAgent) || !/[⌘⇧⌥]/.test(keys)) {
    return keys;
  }
  return [...keys].map((key) => OTHER_PLATFORM_KEYS[key] ?? key).join("+");
}
