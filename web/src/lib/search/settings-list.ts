import { parseEnum, parseString } from "./common";

/** URL state of a settings list page: search, sort, the open row and an optional tab. */
export type SettingsListSearch = {
  q?: string;
  sort?: string;
  desc?: boolean;
  open?: string;
  tab?: string;
};

const QUERY_MAX = 200;
const ID_MAX = 256;

/** Parses the list URL params. `sorts` and `tabs` list the allowed values. Absent keys are omitted. */
export function parseSettingsListSearch(
  search: Record<string, unknown>,
  sorts: readonly string[],
  tabs: readonly string[] = [],
): SettingsListSearch {
  const q = parseString(search.q, QUERY_MAX);
  const sort = parseEnum(search.sort, sorts);
  const desc = search.desc === true || search.desc === "true";
  const open = parseString(search.open, ID_MAX);
  const tab = parseEnum(search.tab, tabs);
  return {
    ...(q ? { q } : {}),
    ...(sort ? { sort } : {}),
    ...(desc ? { desc: true } : {}),
    ...(open ? { open } : {}),
    ...(tab ? { tab } : {}),
  };
}
