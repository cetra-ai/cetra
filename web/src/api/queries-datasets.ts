import { $api } from "./client";
import { projectRef } from "./project-ref";

export function datasetsInit(org: string, project: string, q?: string) {
  return {
    params: {
      path: { project: projectRef(org, project) },
      query: { limit: 100, ...(q ? { q } : {}) },
    },
  };
}

/** Prefix of every dataset list query of the project, for invalidation. */
export function datasetsKey(org: string, project: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/datasets", {
    params: { path: { project: projectRef(org, project) } },
  }).queryKey;
}

export function datasetQuery(org: string, project: string, dataset: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/datasets/{dataset}", {
    params: { path: { project: projectRef(org, project), dataset } },
  });
}

export function datasetKey(org: string, project: string, dataset: string) {
  return datasetQuery(org, project, dataset).queryKey;
}

/** One page request of a version's items; `version` undefined reads the latest. */
export function datasetItemsInit(org: string, project: string, dataset: string, version?: number) {
  return {
    params: {
      path: { project: projectRef(org, project), dataset },
      query: { limit: 100, ...(version !== undefined ? { version } : {}) },
    },
  };
}

/** Prefix of every item list of one dataset, for invalidation. */
export function datasetItemsKey(org: string, project: string, dataset: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/datasets/{dataset}/items", {
    params: { path: { project: projectRef(org, project), dataset } },
  }).queryKey;
}

export function datasetVersionsInit(org: string, project: string, dataset: string) {
  return {
    params: { path: { project: projectRef(org, project), dataset }, query: { limit: 100 } },
  };
}

/** Prefix of every version list of one dataset, for invalidation. */
export function datasetVersionsKey(org: string, project: string, dataset: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/datasets/{dataset}/versions", {
    params: { path: { project: projectRef(org, project), dataset } },
  }).queryKey;
}

/** Every key a dataset write makes stale: the list, the dataset, its items and its versions. */
export function datasetWriteKeys(org: string, project: string, dataset: string) {
  return [
    datasetsKey(org, project),
    datasetKey(org, project, dataset),
    datasetItemsKey(org, project, dataset),
    datasetVersionsKey(org, project, dataset),
  ];
}
