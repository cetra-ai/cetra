import { STORAGE_PREFIX } from "@/product";

export type LastProject = { slug: string; name: string };

function keyOf(org: string): string {
  return `${STORAGE_PREFIX}settings.last_project.${org}`;
}

/** Remembers the project whose settings or pages were last open in an organization. */
export function rememberLastProject(org: string, project: LastProject): void {
  try {
    localStorage.setItem(keyOf(org), JSON.stringify(project));
  } catch {
    // Storage can be blocked or full; the sidebar then simply has no remembered project.
  }
}

/** The project last remembered for the organization, or undefined when none is valid. */
export function readLastProject(org: string): LastProject | undefined {
  let raw: string | null;
  try {
    raw = localStorage.getItem(keyOf(org));
  } catch {
    return undefined;
  }
  if (!raw) {
    return undefined;
  }
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object") {
      return undefined;
    }
    const { slug, name } = parsed as Record<string, unknown>;
    if (typeof slug !== "string" || typeof name !== "string" || slug === "" || name === "") {
      return undefined;
    }
    return { slug, name };
  } catch {
    return undefined;
  }
}
