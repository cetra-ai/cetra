/**
 * Annotation queues over REST (spec 10 § Datasets and experiments, D-AQ-8).
 *
 * `client.annotationQueues` lists, reads, creates, updates and deletes queues,
 * adds traces, spans or sessions to them, and lists their items. The reviewer
 * actions are web only (D-AQ-11). Wire names are snake_case; the public names
 * are camelCase.
 */

import type { Client } from "./client.js";
import { segment } from "./client.js";
import { collect, pageQuery, type WirePage } from "./datasets.js";

/** Targets per `POST /annotation-queues/{queue}/items` request. */
const ADD_BATCH_SIZE = 1_000;
const TARGET_KINDS = ["traceIds", "spans", "sessionIds"] as const;
const TARGET_NAMES: ReadonlySet<string> = new Set(["trace", "span", "session"]);
const KIND_MESSAGE = "give exactly one of traceIds, spans or sessionIds";

/** What a queue's items are (D-AQ-2). */
export type QueueTarget = "trace" | "span" | "session";
/** Where an item is in its review (D-AQ-5). */
export type QueueItemStatus = "pending" | "completed" | "skipped";

/** Item counts of a queue, by status. */
export interface QueueCounts {
  readonly pending: number;
  readonly completed: number;
  readonly skipped: number;
}

/** An annotation queue with its counts. Absent optional values are `undefined`. */
export interface AnnotationQueue {
  readonly id: string;
  readonly name: string;
  readonly description: string | undefined;
  readonly instructions: string | undefined;
  readonly target: QueueTarget;
  readonly definitionIds: readonly string[];
  /** Incremented by every change; `update` names the version it read. */
  readonly version: number;
  readonly createdBy: string | undefined;
  /** RFC 3339, as the dataset types keep times. */
  readonly createdAt: string;
  readonly updatedAt: string;
  readonly counts: QueueCounts;
}

/** One target of a queue. Absent optional values are `undefined`. */
export interface AnnotationQueueItem {
  readonly id: string;
  readonly queueId: string;
  readonly traceId: string | undefined;
  readonly spanId: string | undefined;
  readonly sessionId: string | undefined;
  readonly targetTime: string;
  readonly status: QueueItemStatus;
  readonly skipReason: string | undefined;
  readonly leasedByUserId: string | undefined;
  readonly leaseExpiresAt: string | undefined;
  readonly doneByUserId: string | undefined;
  readonly doneAt: string | undefined;
  readonly addedBy: "user" | "pat" | "key";
  readonly addedById: string;
  readonly createdAt: string;
}

/** A target that was not added, and why. `index` is its position in the caller's list. */
export interface SkippedQueueItem {
  readonly index: number;
  readonly message: string;
}

/** The answer of `addItems`. A target already in the queue counts as `existing`. */
export interface AddedQueueItems {
  readonly added: number;
  readonly existing: number;
  readonly skipped: readonly SkippedQueueItem[];
}

/** The targets of `addItems`: exactly one of the three keys. */
export type QueueTargets =
  | { readonly traceIds: readonly string[] }
  | { readonly spans: readonly (readonly [string, string])[] }
  | { readonly sessionIds: readonly string[] };

interface WireQueue {
  id: string;
  name: string;
  description?: string;
  instructions?: string;
  target: QueueTarget;
  definition_ids: string[];
  version: number;
  created_by?: string;
  created_at: string;
  updated_at: string;
  counts: { pending: number; completed: number; skipped: number };
}

interface WireQueueItem {
  id: string;
  queue_id: string;
  trace_id?: string;
  span_id?: string;
  session_id?: string;
  target_time: string;
  status: QueueItemStatus;
  skip_reason?: string;
  leased_by_user_id?: string;
  lease_expires_at?: string;
  done_by_user_id?: string;
  done_at?: string;
  added_by: "user" | "pat" | "key";
  added_by_id: string;
  created_at: string;
}

interface WireAdded {
  added: number;
  existing: number;
  skipped: { index: number; message: string }[];
}

/** `client.annotationQueues`. */
export class AnnotationQueues {
  readonly #client: Client;

  /** @internal */
  constructor(client: Client) {
    this.#client = client;
  }

  /** Every queue of the key's project. */
  async list(): Promise<AnnotationQueue[]> {
    return collect(
      (cursor) =>
        this.#client.request<WirePage<WireQueue>>(
          "GET",
          "/annotation-queues",
          "/annotation-queues",
          { query: pageQuery({}, cursor) },
        ),
      queueFromWire,
    );
  }

  /** One queue by id. */
  async get(queueId: string): Promise<AnnotationQueue> {
    const wire = await this.#client.request<WireQueue>(
      "GET",
      "/annotation-queues/{queue}",
      `/annotation-queues/${segment(queueId)}`,
    );
    return queueFromWire(wire);
  }

  /** Create a queue. Sent once: a retry could create a second one. */
  async create(
    name: string,
    options: {
      target: QueueTarget;
      definitionIds: readonly string[];
      description?: string;
      instructions?: string;
    },
  ): Promise<AnnotationQueue> {
    if (!TARGET_NAMES.has(options.target)) {
      throw new TypeError("target must be trace, span or session");
    }
    if (options.definitionIds.length === 0) {
      throw new TypeError("definitionIds: give at least one score definition");
    }
    const body: Record<string, unknown> = {
      name,
      target: options.target,
      definition_ids: [...options.definitionIds],
    };
    if (options.description !== undefined) {
      body.description = options.description;
    }
    if (options.instructions !== undefined) {
      body.instructions = options.instructions;
    }
    const wire = await this.#client.request<WireQueue>(
      "POST",
      "/annotation-queues",
      "/annotation-queues",
      { body, retry: false },
    );
    return queueFromWire(wire);
  }

  /**
   * Change a queue. `version` is the version the caller read; the server
   * answers `409` when it is stale. `""` clears a text field.
   */
  async update(
    queueId: string,
    options: {
      version: number;
      name?: string;
      description?: string;
      instructions?: string;
      definitionIds?: readonly string[];
    },
  ): Promise<AnnotationQueue> {
    const body: Record<string, unknown> = { version: options.version };
    if (options.name !== undefined) {
      body.name = options.name;
    }
    if (options.description !== undefined) {
      body.description = options.description;
    }
    if (options.instructions !== undefined) {
      body.instructions = options.instructions;
    }
    if (options.definitionIds !== undefined) {
      body.definition_ids = [...options.definitionIds];
    }
    const wire = await this.#client.request<WireQueue>(
      "PATCH",
      "/annotation-queues/{queue}",
      `/annotation-queues/${segment(queueId)}`,
      { body, retry: false },
    );
    return queueFromWire(wire);
  }

  /** Delete a queue and its items. */
  async delete(queueId: string): Promise<void> {
    await this.#client.request<void>(
      "DELETE",
      "/annotation-queues/{queue}",
      `/annotation-queues/${segment(queueId)}`,
    );
  }

  /**
   * Add traces, spans or sessions to a queue, in batches of 1,000 (D-AQ-8).
   * Rejects with `TypeError` before any request when the targets are malformed.
   * `skipped` indexes are positions in the list the caller passed. An empty
   * list resolves to zero counts without a request.
   */
  async addItems(queueId: string, targets: QueueTargets): Promise<AddedQueueItems> {
    const items = targetsToWire(targets);
    let added = 0;
    let existing = 0;
    const skipped: SkippedQueueItem[] = [];
    for (let start = 0; start < items.length; start += ADD_BATCH_SIZE) {
      const answer = await this.#client.request<WireAdded>(
        "POST",
        "/annotation-queues/{queue}/items",
        `/annotation-queues/${segment(queueId)}/items`,
        {
          body: { items: items.slice(start, start + ADD_BATCH_SIZE) },
          retry: true,
        },
      );
      added += answer.added;
      existing += answer.existing;
      for (const entry of answer.skipped) {
        skipped.push({ index: entry.index + start, message: entry.message });
      }
    }
    return { added, existing, skipped };
  }

  /** The items of a queue, optionally only one status. Every page is read. */
  async items(
    queueId: string,
    options: { status?: QueueItemStatus } = {},
  ): Promise<AnnotationQueueItem[]> {
    const status = options.status;
    return collect(
      (cursor) =>
        this.#client.request<WirePage<WireQueueItem>>(
          "GET",
          "/annotation-queues/{queue}/items",
          `/annotation-queues/${segment(queueId)}/items`,
          {
            query: {
              ...(status === undefined ? {} : { status }),
              ...pageQuery({}, cursor),
            },
          },
        ),
      queueItemFromWire,
    );
  }
}

function queueFromWire(wire: WireQueue): AnnotationQueue {
  return {
    id: wire.id,
    name: wire.name,
    description: wire.description,
    instructions: wire.instructions,
    target: wire.target,
    definitionIds: wire.definition_ids,
    version: wire.version,
    createdBy: wire.created_by,
    createdAt: wire.created_at,
    updatedAt: wire.updated_at,
    counts: {
      pending: wire.counts.pending,
      completed: wire.counts.completed,
      skipped: wire.counts.skipped,
    },
  };
}

function queueItemFromWire(wire: WireQueueItem): AnnotationQueueItem {
  return {
    id: wire.id,
    queueId: wire.queue_id,
    traceId: wire.trace_id,
    spanId: wire.span_id,
    sessionId: wire.session_id,
    targetTime: wire.target_time,
    status: wire.status,
    skipReason: wire.skip_reason,
    leasedByUserId: wire.leased_by_user_id,
    leaseExpiresAt: wire.lease_expires_at,
    doneByUserId: wire.done_by_user_id,
    doneAt: wire.done_at,
    addedBy: wire.added_by,
    addedById: wire.added_by_id,
    createdAt: wire.created_at,
  };
}

/**
 * The wire targets of `addItems`. Throws `TypeError` naming the problem, never
 * a value, before any request is sent.
 */
function targetsToWire(targets: QueueTargets): Record<string, string>[] {
  if (typeof targets !== "object" || targets === null || Array.isArray(targets)) {
    throw new TypeError(KIND_MESSAGE);
  }
  const record = targets as unknown as Record<string, unknown>;
  for (const key of Object.keys(record)) {
    if (!(TARGET_KINDS as readonly string[]).includes(key)) {
      throw new TypeError(`unknown key ${key}`);
    }
  }
  const given = TARGET_KINDS.filter((key) => record[key] !== undefined);
  const only = given[0];
  if (given.length !== 1 || only === undefined) {
    throw new TypeError(KIND_MESSAGE);
  }
  switch (only) {
    case "traceIds":
      return idList(record.traceIds, "traceIds").map((id) => ({ trace_id: id }));
    case "sessionIds":
      return idList(record.sessionIds, "sessionIds").map((id) => ({ session_id: id }));
    case "spans":
      return spanList(record.spans);
  }
}

/** The ids of a `traceIds` or `sessionIds` list; a bare string is refused. */
function idList(value: unknown, name: string): string[] {
  if (typeof value === "string") {
    throw new TypeError(`${name} must be an array of ids, not a string`);
  }
  if (!Array.isArray(value)) {
    throw new TypeError(`${name} must be an array of ids`);
  }
  const list: unknown[] = value;
  return list.map((id, index) => {
    if (typeof id !== "string") {
      throw new TypeError(`${name}[${String(index)}]: must be a string`);
    }
    return id;
  });
}

/** The wire items of a `spans` list: each entry a `[traceId, spanId]` pair. */
function spanList(value: unknown): Record<string, string>[] {
  if (!Array.isArray(value)) {
    throw new TypeError("spans must be an array of [traceId, spanId] pairs");
  }
  const list: unknown[] = value;
  return list.map((pair, index) => spanWire(pair, index));
}

function spanWire(pair: unknown, index: number): Record<string, string> {
  if (Array.isArray(pair) && pair.length === 2) {
    const [traceId, spanId] = pair;
    if (typeof traceId === "string" && typeof spanId === "string") {
      return { trace_id: traceId, span_id: spanId };
    }
  }
  throw new TypeError(`spans[${String(index)}]: must be a [traceId, spanId] pair of strings`);
}
