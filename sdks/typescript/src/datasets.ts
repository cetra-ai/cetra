/**
 * Datasets over REST (spec 10 § Datasets and experiments, D-J-2, D-J-3).
 *
 * `client.datasets` creates, reads, lists and deletes datasets; `Dataset.addItems`
 * adds items in batches of 1,000, each batch one new version. Wire names are
 * snake_case; the public names are camelCase.
 */

import type { Client } from "./client.js";
import { segment } from "./client.js";
import { toJsonable } from "./serialize.js";

/** Page size of the list and item routes. */
const PAGE_SIZE = 1_000;
/** Items per `POST /datasets/{dataset}/items` request. */
const ADD_BATCH_SIZE = 1_000;
const ITEM_KEYS: ReadonlySet<string> = new Set(["input", "expectedOutput", "metadata"]);

/** One item of a dataset version (D-J-2). Absent optional values are `undefined`. */
export interface DatasetItem {
  readonly id: string;
  readonly input: unknown;
  readonly expectedOutput: unknown;
  readonly metadata: Readonly<Record<string, unknown>>;
  /** The version this revision appeared in. */
  readonly version: number;
  readonly sourceTraceId: string | undefined;
  readonly sourceSpanId: string | undefined;
}

/** A dataset without its items, as listed. */
export interface DatasetInfo {
  readonly id: string;
  readonly name: string;
  readonly description: string | undefined;
  readonly currentVersion: number;
  readonly itemCount: number;
}

/** An item to add: `input` is required. */
export interface NewDatasetItem {
  input: unknown;
  expectedOutput?: unknown;
  metadata?: Readonly<Record<string, unknown>>;
}

/** One version of a dataset with all its items. */
export class Dataset {
  readonly id: string;
  readonly name: string;
  readonly description: string | undefined;
  /** The version these items belong to. */
  readonly version: number;
  readonly items: readonly DatasetItem[];
  readonly #client: Client;

  /** @internal */
  constructor(
    fields: {
      id: string;
      name: string;
      description: string | undefined;
      version: number;
      items: readonly DatasetItem[];
    },
    client: Client,
  ) {
    this.id = fields.id;
    this.name = fields.name;
    this.description = fields.description;
    this.version = fields.version;
    this.items = fields.items;
    this.#client = client;
  }

  /**
   * Add items; each batch of up to 1,000 makes one new version (D-J-3).
   * Rejects with `TypeError` before any request when an item is malformed.
   * Resolves to the latest version; an empty iterable resolves to `this`.
   */
  async addItems(items: Iterable<NewDatasetItem>): Promise<Dataset> {
    const all = [...items];
    all.forEach(checkItem);
    if (all.length === 0) {
      return this;
    }
    for (let start = 0; start < all.length; start += ADD_BATCH_SIZE) {
      const batch = all.slice(start, start + ADD_BATCH_SIZE).map(itemToWire);
      await this.#client.request(
        "POST",
        "/datasets/{dataset}/items",
        `/datasets/${segment(this.id)}/items`,
        {
          body: { items: batch },
          retry: false,
        },
      );
    }
    return this.#client.datasets.get(this.id);
  }
}

/** `client.datasets`. */
export class Datasets {
  readonly #client: Client;

  /** @internal */
  constructor(client: Client) {
    this.#client = client;
  }

  /** Create a dataset (version 0, no items). Sent once: a retry could create a second one. */
  async create(
    name: string,
    options: {
      description?: string;
      inputSchema?: Record<string, unknown>;
      expectedSchema?: Record<string, unknown>;
    } = {},
  ): Promise<Dataset> {
    const body: Record<string, unknown> = { name };
    if (options.description !== undefined) {
      body.description = options.description;
    }
    if (options.inputSchema !== undefined) {
      body.input_schema = options.inputSchema;
    }
    if (options.expectedSchema !== undefined) {
      body.expected_schema = options.expectedSchema;
    }
    const wire = await this.#client.request<WireDataset>("POST", "/datasets", "/datasets", {
      body,
      retry: false,
    });
    return new Dataset(
      { id: wire.id, name: wire.name, description: wire.description, version: 0, items: [] },
      this.#client,
    );
  }

  /** A dataset with the items of `options.version` (default: the current version). */
  async get(nameOrId: string, options: { version?: number } = {}): Promise<Dataset> {
    const wire = await this.#client.request<WireDataset>(
      "GET",
      "/datasets/{dataset}",
      `/datasets/${segment(nameOrId)}`,
    );
    const version = options.version ?? wire.current_version;
    const items = await collect(
      (cursor) =>
        this.#client.request<WirePage<WireItem>>(
          "GET",
          "/datasets/{dataset}/items",
          `/datasets/${segment(wire.id)}/items`,
          { query: pageQuery({ version }, cursor) },
        ),
      itemFromWire,
    );
    return new Dataset(
      {
        id: wire.id,
        name: wire.name,
        description: wire.description,
        version,
        items,
      },
      this.#client,
    );
  }

  /** Every dataset, without items. */
  async list(): Promise<DatasetInfo[]> {
    return collect(
      (cursor) =>
        this.#client.request<WirePage<WireDataset>>("GET", "/datasets", "/datasets", {
          query: pageQuery({}, cursor),
        }),
      infoFromWire,
    );
  }

  /** Delete a dataset by name or id. */
  async delete(nameOrId: string): Promise<void> {
    await this.#client.request<void>(
      "DELETE",
      "/datasets/{dataset}",
      `/datasets/${segment(nameOrId)}`,
    );
  }
}

interface WireDataset {
  id: string;
  name: string;
  description?: string;
  current_version: number;
  item_count: number;
}

interface WireItem {
  id: string;
  input: unknown;
  expected_output?: unknown;
  metadata?: Record<string, unknown>;
  version: number;
  source_trace_id?: string;
  source_span_id?: string;
}

interface WirePage<T> {
  data: T[];
  next_cursor: string | null;
}

/** Query of one page: the given fields, then `limit`, then `cursor` when there is one. */
function pageQuery(
  fields: Record<string, number>,
  cursor: string | undefined,
): Record<string, string | number> {
  const query: Record<string, string | number> = { ...fields, limit: PAGE_SIZE };
  if (cursor !== undefined) {
    query.cursor = cursor;
  }
  return query;
}

/** Fetch pages until `next_cursor` is null, mapping every item in order. */
async function collect<W, T>(
  fetchPage: (cursor: string | undefined) => Promise<WirePage<W>>,
  map: (wire: W) => T,
): Promise<T[]> {
  const out: T[] = [];
  let cursor: string | undefined;
  for (;;) {
    const page = await fetchPage(cursor);
    out.push(...page.data.map(map));
    if (page.next_cursor === null) {
      return out;
    }
    cursor = page.next_cursor;
  }
}

function infoFromWire(wire: WireDataset): DatasetInfo {
  return {
    id: wire.id,
    name: wire.name,
    description: wire.description,
    currentVersion: wire.current_version,
    itemCount: wire.item_count,
  };
}

function itemFromWire(wire: WireItem): DatasetItem {
  return {
    id: wire.id,
    input: wire.input,
    expectedOutput: wire.expected_output ?? undefined,
    metadata: wire.metadata ?? {},
    version: wire.version,
    sourceTraceId: wire.source_trace_id,
    sourceSpanId: wire.source_span_id,
  };
}

/** Throws `TypeError` naming the item's index and the problem, never the value. */
function checkItem(item: unknown, index: number): void {
  if (typeof item !== "object" || item === null || Array.isArray(item)) {
    throw new TypeError(`items[${String(index)}]: must be an object`);
  }
  for (const key of Object.keys(item)) {
    if (!ITEM_KEYS.has(key)) {
      throw new TypeError(`items[${String(index)}]: unknown key ${key}`);
    }
  }
  const record = item as Record<string, unknown>;
  if (!("input" in record) || record.input === undefined) {
    throw new TypeError(`items[${String(index)}].input: required`);
  }
  const metadata = record.metadata;
  if (
    metadata !== undefined &&
    (typeof metadata !== "object" || metadata === null || Array.isArray(metadata))
  ) {
    throw new TypeError(`items[${String(index)}].metadata: must be an object`);
  }
}

function itemToWire(item: NewDatasetItem): Record<string, unknown> {
  const wire: Record<string, unknown> = { input: toJsonable(item.input) };
  if (item.expectedOutput !== undefined) {
    wire.expected_output = toJsonable(item.expectedOutput);
  }
  if (item.metadata !== undefined) {
    wire.metadata = toJsonable(item.metadata);
  }
  return wire;
}
