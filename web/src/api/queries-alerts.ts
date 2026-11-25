import { $api } from "@/api/client";
import { projectRef } from "@/api/project-ref";
import type { Schema } from "@/api/types";

export function channelsInit(org: string, project: string) {
  return { params: { path: { project: projectRef(org, project) } } };
}

export function channelsQuery(org: string, project: string) {
  return $api.queryOptions(
    "get",
    "/api/v1/projects/{project}/channels",
    channelsInit(org, project),
  );
}

export function alertRulesQuery(org: string, project: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/alert-rules", {
    params: { path: { project: projectRef(org, project) } },
  });
}

export function alertRuleQuery(org: string, project: string, id: string) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/alert-rules/{id}", {
    params: { path: { project: projectRef(org, project), id } },
  });
}

export function alertEventsInit(org: string, project: string, id: string) {
  return { params: { path: { project: projectRef(org, project), id }, query: { limit: 50 } } };
}

export function projectAlertEventsInit(org: string, project: string) {
  return { params: { path: { project: projectRef(org, project) }, query: { limit: 20 } } };
}

/** The issue or source an issue occurrence rule counts (exactly one). */
export type IssueAlertScope = { issue_id: string } | { source_id: string };

export function issueAlertPreviewQuery(
  org: string,
  project: string,
  scope: IssueAlertScope,
  window: Schema<"AlertWindow">,
) {
  return $api.queryOptions("get", "/api/v1/projects/{project}/alert-rules/issue-preview", {
    params: { path: { project: projectRef(org, project) }, query: { ...scope, window } },
  });
}
