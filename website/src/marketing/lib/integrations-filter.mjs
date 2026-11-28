/** @typedef {"python" | "typescript" | "java" | "kotlin" | "dotnet"} Language */

/**
 * @typedef {object} FilterItem
 * @property {string} group
 * @property {string} title
 * @property {string} description
 * @property {Language[]} languages empty: used from any language
 */

/** The language chips, in order. `matches` empty: no language filter. */
export const LANGUAGE_FILTERS = [
  { id: "any", label: "Any language", matches: [] },
  { id: "python", label: "Python", matches: ["python"] },
  { id: "typescript", label: "TypeScript", matches: ["typescript"] },
  { id: "jvm", label: "JVM", matches: ["java", "kotlin"] },
  { id: "dotnet", label: ".NET", matches: ["dotnet"] },
];

/**
 * Which items show.
 * @param {FilterItem[]} items
 * @param {{ group: string | null, language: string, query: string }} filter
 *   group null: every group; language: a `LANGUAGE_FILTERS` id; query: the search box's text
 * @returns {{ matched: number[], anyLanguage: number[] }} indexes into `items`, ascending.
 *   `matched`: the main grid. `anyLanguage`: the "Works with any language" strip, always empty
 *   when `language` is "any".
 */
export function filterIntegrations(items, filter) {
  const language = LANGUAGE_FILTERS.find((entry) => entry.id === filter.language);
  if (!language) {
    throw new Error(`unknown language filter: ${filter.language}`);
  }
  const tokens = filter.query
    .trim()
    .toLowerCase()
    .split(/\s+/)
    .filter((token) => token !== "");
  /** @type {number[]} */
  const matched = [];
  /** @type {number[]} */
  const anyLanguage = [];
  items.forEach((item, i) => {
    if (filter.group !== null && item.group !== filter.group) {
      return;
    }
    const haystack = `${item.title} ${item.description} ${item.group}`.toLowerCase();
    if (!tokens.every((token) => haystack.includes(token))) {
      return;
    }
    if (language.matches.length === 0) {
      matched.push(i);
    } else if (item.languages.length === 0) {
      anyLanguage.push(i);
    } else if (item.languages.some((value) => language.matches.includes(value))) {
      matched.push(i);
    }
  });
  return { matched, anyLanguage };
}
