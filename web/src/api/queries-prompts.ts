import type { QueryClient, QueryKey } from "@tanstack/react-query";
import { $api } from "./client";
import { projectRef } from "./project-ref";

const VERSION_PATH = "/api/v1/projects/{project}/prompts/{prompt}/versions/{version}";

export function promptsInit(
  org: string,
  project: string,
  filters: { q?: string; label?: string; tag?: string },
) {
  return {
    params: {
      path: { project: projectRef(org, project) },
      query: {
        limit: 100,
        ...(filters.q ? { q: filters.q } : {}),
        ...(filters.label ? { label: filters.label } : {}),
        ...(filters.tag ? { tag: filters.tag } : {}),
      },
    },
  };
}

/** Prefix of every prompt list query of the project, for invalidation. */
export function promptsKey(org: string, project: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/prompts", {
    params: { path: { project: projectRef(org, project) } },
  }).queryKey;
}

export function promptQuery(org: string, project: string, prompt: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/prompts/{prompt}", {
    params: { path: { project: projectRef(org, project), prompt } },
  });
}

export function promptKey(org: string, project: string, prompt: string) {
  return promptQuery(org, project, prompt).queryKey;
}

export function promptVersionsInit(org: string, project: string, prompt: string) {
  return {
    params: { path: { project: projectRef(org, project), prompt }, query: { limit: 50 } },
  };
}

/** Prefix of every version list of one prompt, for invalidation. */
export function promptVersionsKey(org: string, project: string, prompt: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/prompts/{prompt}/versions", {
    params: { path: { project: projectRef(org, project), prompt } },
  }).queryKey;
}

export function promptVersionQuery(
  org: string,
  project: string,
  prompt: string,
  version: number,
) {
  return $api.queryOptions("get", VERSION_PATH, {
    params: { path: { project: projectRef(org, project), prompt, version } },
  });
}

export function promptLabelEventsInit(org: string, project: string, prompt: string) {
  return {
    params: { path: { project: projectRef(org, project), prompt }, query: { limit: 100 } },
  };
}

/** Prefix of every label history query of one prompt, for invalidation. */
export function promptLabelEventsKey(org: string, project: string, prompt: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/prompts/{prompt}/label-events", {
    params: { path: { project: projectRef(org, project), prompt } },
  }).queryKey;
}

export function promptSettingsQuery(org: string, project: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/prompt-settings", {
    params: { path: { project: projectRef(org, project) } },
  });
}

export function promptSettingsKey(org: string, project: string) {
  return promptSettingsQuery(org, project).queryKey;
}

export function promptMetricsQuery(
  org: string,
  project: string,
  prompt: string,
  from: string,
  to: string,
) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/prompts/{prompt}/metrics", {
    params: { path: { project: projectRef(org, project), prompt }, query: { from, to } },
  });
}

/**
 * Every key a prompt write can change: the list, the prompt, its versions and its label history.
 * Single-version queries are not listed here; `invalidatePrompt` covers them too.
 */
export function promptWriteKeys(org: string, project: string, prompt: string): QueryKey[] {
  return [
    promptsKey(org, project),
    promptKey(org, project, prompt),
    promptVersionsKey(org, project, prompt),
    promptLabelEventsKey(org, project, prompt),
  ];
}

/** Marks stale, after a prompt write, every query that the write can change. */
export function invalidatePrompt(
  queryClient: QueryClient,
  org: string,
  project: string,
  prompt: string,
) {
  for (const queryKey of promptWriteKeys(org, project, prompt)) {
    void queryClient.invalidateQueries({ queryKey });
  }
  void queryClient.invalidateQueries({
    predicate: (query) => query.queryKey[1] === VERSION_PATH,
  });
}
