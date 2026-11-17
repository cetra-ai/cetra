import { projectRef } from "./project-ref";

/** The trends of some datasets of the project (D-J-26): `ids` are 1 to 100 dataset ids. */
export function datasetTrendsInit(org: string, project: string, ids: readonly string[]) {
  return {
    params: {
      path: { project: projectRef(org, project) },
      query: { ids: ids.join(",") },
    },
  };
}
