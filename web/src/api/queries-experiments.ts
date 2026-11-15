import { $api } from "./client";
import { projectRef } from "./project-ref";

export function experimentsInit(org: string, project: string, dataset?: string) {
  return {
    params: {
      path: { project: projectRef(org, project) },
      query: { limit: 100, ...(dataset ? { dataset } : {}) },
    },
  };
}

/** Prefix of every experiment list query of the project, for invalidation. */
export function experimentsKey(org: string, project: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/experiments", {
    params: { path: { project: projectRef(org, project) } },
  }).queryKey;
}

export function experimentQuery(org: string, project: string, id: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/experiments/{id}", {
    params: { path: { project: projectRef(org, project), id } },
  });
}

export function experimentResultsInit(
  org: string,
  project: string,
  id: string,
  errorsOnly: boolean,
) {
  return {
    params: {
      path: { project: projectRef(org, project), id },
      query: { limit: 100, ...(errorsOnly ? { errors_only: true } : {}) },
    },
  };
}

export function compareInit(
  org: string,
  project: string,
  ids: readonly string[],
  threshold: number,
  regressionsOnly: boolean,
) {
  return {
    params: {
      path: { project: projectRef(org, project) },
      query: {
        ids: ids.join(","),
        threshold,
        limit: 100,
        ...(regressionsOnly ? { regressions_only: true } : {}),
      },
    },
  };
}
