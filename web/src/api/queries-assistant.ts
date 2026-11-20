import { $api } from "./client";
import { projectRef } from "./project-ref";

export function assistantStatusInit(org: string, project: string) {
  return { params: { path: { project: projectRef(org, project) } } };
}

export function assistantStatusQuery(org: string, project: string) {
  return $api.queryOptions(
    "get",
    "/api/v1/projects/{project}/assistant",
    assistantStatusInit(org, project),
  );
}

export function assistantStatusKey(org: string, project: string) {
  return assistantStatusQuery(org, project).queryKey;
}

export function conversationsInit(org: string, project: string) {
  return { params: { path: { project: projectRef(org, project) } } };
}

export function conversationsQuery(org: string, project: string) {
  return $api.queryOptions(
    "get",
    "/api/v1/projects/{project}/assistant/conversations",
    conversationsInit(org, project),
  );
}

export function conversationsKey(org: string, project: string) {
  return conversationsQuery(org, project).queryKey;
}

export function conversationInit(org: string, project: string, id: string) {
  return { params: { path: { project: projectRef(org, project), id } } };
}

export function conversationQuery(org: string, project: string, id: string) {
  return $api.queryOptions(
    "get",
    "/api/v1/projects/{project}/assistant/conversations/{id}",
    conversationInit(org, project, id),
  );
}

export function conversationKey(org: string, project: string, id: string) {
  return conversationQuery(org, project, id).queryKey;
}

export function orgAssistantInit(org: string) {
  return { params: { path: { org } } };
}

export function orgAssistantQuery(org: string) {
  return $api.queryOptions("get", "/api/v1/orgs/{org}/assistant", orgAssistantInit(org));
}

export function orgAssistantKey(org: string) {
  return orgAssistantQuery(org).queryKey;
}

/** The turn route, a POST answered with `text/event-stream` (spec 09 § Assistant). Not an OpenAPI client call. */
export function turnUrl(org: string, project: string, id: string): string {
  return `/api/v1/projects/${encodeURIComponent(projectRef(org, project))}/assistant/conversations/${id}/turns`;
}
