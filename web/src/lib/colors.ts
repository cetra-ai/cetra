// Stable colours for categories (tags, environments, models' providers). Every class
// string is written out in full so Tailwind can see it in the source.

export interface CategoryClasses {
  text: string;
  dot: string;
  pill: string;
}

export const CATEGORY_CLASSES: readonly CategoryClasses[] = [
  { text: "text-cat-1", dot: "bg-cat-1", pill: "pill [--pill:var(--cat-1)]" },
  { text: "text-cat-2", dot: "bg-cat-2", pill: "pill [--pill:var(--cat-2)]" },
  { text: "text-cat-3", dot: "bg-cat-3", pill: "pill [--pill:var(--cat-3)]" },
  { text: "text-cat-4", dot: "bg-cat-4", pill: "pill [--pill:var(--cat-4)]" },
  { text: "text-cat-5", dot: "bg-cat-5", pill: "pill [--pill:var(--cat-5)]" },
  { text: "text-cat-6", dot: "bg-cat-6", pill: "pill [--pill:var(--cat-6)]" },
  { text: "text-cat-7", dot: "bg-cat-7", pill: "pill [--pill:var(--cat-7)]" },
  { text: "text-cat-8", dot: "bg-cat-8", pill: "pill [--pill:var(--cat-8)]" },
];

const FNV_OFFSET_BASIS = 0x811c9dc5;
const FNV_PRIME = 0x01000193;

/** FNV-1a (32-bit) over the UTF-16 code units of `name`, modulo the palette size. */
export function categoryIndex(name: string): number {
  let hash = FNV_OFFSET_BASIS;
  for (let i = 0; i < name.length; i += 1) {
    hash ^= name.charCodeAt(i);
    hash = Math.imul(hash, FNV_PRIME) >>> 0;
  }
  return hash % CATEGORY_CLASSES.length;
}

export function categoryClasses(name: string): CategoryClasses {
  return CATEGORY_CLASSES[categoryIndex(name)] as CategoryClasses;
}

/**
 * Colours of an environment: production is always the first category colour (the
 * dashboard's blue), and other environments never take it.
 */
export function environmentClasses(environment: string): CategoryClasses {
  const name = environment.toLowerCase();
  if (name === "production" || name === "prod") {
    return CATEGORY_CLASSES[0] as CategoryClasses;
  }
  const index = categoryIndex(environment);
  return CATEGORY_CLASSES[index === 0 ? 1 : index] as CategoryClasses;
}

/** The provider of a model name, from its well-known prefixes or the text before the first `-` or `/`. */
export function modelProvider(model: string): string {
  const lower = model.toLowerCase();
  if (lower.startsWith("claude")) {
    return "anthropic";
  }
  if (["gpt", "o1", "o3", "o4", "text-embedding"].some((prefix) => lower.startsWith(prefix))) {
    return "openai";
  }
  if (lower.startsWith("gemini") || lower.startsWith("gemma")) {
    return "google";
  }
  if (["mistral", "mixtral", "codestral"].some((prefix) => lower.startsWith(prefix))) {
    return "mistral";
  }
  if (lower.startsWith("llama")) {
    return "meta";
  }
  if (lower.startsWith("command")) {
    return "cohere";
  }
  if (lower.startsWith("deepseek")) {
    return "deepseek";
  }
  const cut = lower.search(/[-/]/);
  return cut === -1 ? lower : lower.slice(0, cut);
}

const PROVIDER_INDEX: Record<string, number> = {
  anthropic: 5,
  openai: 1,
  google: 0,
  mistral: 2,
  meta: 3,
  cohere: 4,
  deepseek: 7,
};

export function providerClasses(model: string): CategoryClasses {
  const provider = modelProvider(model);
  const fixed = PROVIDER_INDEX[provider];
  if (fixed !== undefined) {
    return CATEGORY_CLASSES[fixed] as CategoryClasses;
  }
  return categoryClasses(provider);
}
