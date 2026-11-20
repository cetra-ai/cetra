import { $api } from "./client";
import { projectRef } from "./project-ref";
import type { Schema } from "./types";

const QUEUES = "/api/v1/projects/{project}/annotation-queues";
const QUEUE = "/api/v1/projects/{project}/annotation-queues/{id}";
const ITEMS = "/api/v1/projects/{project}/annotation-queues/{id}/items";
const ITEM = "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}";

export function annotationQueuesInit(org: string, project: string) {
  return { params: { path: { project: projectRef(org, project) }, query: { limit: 100 } } };
}

/** Prefix of every queue list query of the project, for invalidation. */
export function annotationQueuesKey(org: string, project: string) {
  return $api.queryOptions("get", QUEUES, {
    params: { path: { project: projectRef(org, project) } },
  }).queryKey;
}

export function annotationQueueQuery(org: string, project: string, id: string) {
  return $api.queryOptions("get", QUEUE, {
    params: { path: { project: projectRef(org, project), id } },
  });
}

export function annotationQueueKey(org: string, project: string, id: string) {
  return annotationQueueQuery(org, project, id).queryKey;
}

/** One page request of a queue's items, optionally only those with one status. */
export function annotationQueueItemsInit(
  org: string,
  project: string,
  id: string,
  status?: Schema<"QueueItemStatus">,
) {
  return {
    params: {
      path: { project: projectRef(org, project), id },
      query: { limit: 100, ...(status ? { status } : {}) },
    },
  };
}

/** Prefix of every item list of one queue, for invalidation. */
export function annotationQueueItemsKey(org: string, project: string, id: string) {
  return $api.queryOptions("get", ITEMS, {
    params: { path: { project: projectRef(org, project), id } },
  }).queryKey;
}

export function annotationQueueItemQuery(org: string, project: string, id: string, itemId: string) {
  return $api.queryOptions("get", ITEM, {
    params: { path: { project: projectRef(org, project), id, item_id: itemId } },
  });
}

/**
 * The queues that hold an item of one trace or session, for the "In n queues" chip. A plain
 * query, never the list's infinite init: both are `GET …/annotation-queues`.
 */
export function queuesHoldingQuery(
  org: string,
  project: string,
  target: { traceId: string } | { sessionId: string },
) {
  const filter =
    "traceId" in target
      ? { target_trace_id: target.traceId }
      : { target_session_id: target.sessionId };
  return $api.queryOptions("get", QUEUES, {
    params: { path: { project: projectRef(org, project) }, query: { limit: 100, ...filter } },
  });
}

/**
 * Every key a queue write makes stale: the queue list, the queue (when `id` is given), its
 * items and its item queries. The item queries are keyed by the item's path, so they are
 * matched by the path template and the queue's path parameters.
 */
export function annotationQueueWriteKeys(org: string, project: string, id?: string) {
  const keys: (readonly unknown[])[] = [annotationQueuesKey(org, project)];
  if (id !== undefined) {
    keys.push(annotationQueueKey(org, project, id), annotationQueueItemsKey(org, project, id));
    keys.push(["get", ITEM, { params: { path: { project: projectRef(org, project), id } } }]);
  }
  return keys;
}
