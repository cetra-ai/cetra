import { projectNav } from "./nav";

/** The project page a path is on: the nav entry whose path is the path or a prefix of it ending at a `/`. */
export function pageOf(
  pathname: string,
  org: string,
  project: string,
): (typeof projectNav)[number] | undefined {
  return projectNav.find((entry) => {
    const base = entry.to.replace("$org", org).replace("$project", project);
    return pathname === base || pathname.startsWith(`${base}/`);
  });
}
