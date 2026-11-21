import { api } from "./client";
import type { Schema } from "./types";

/**
 * Fields asked within this many milliseconds share one `POST /facets` (D-C-24). One frame is
 * enough for the sections and preset chips a page mounts together.
 */
export const FACET_BATCH_MS = 10;

/** Most fields in one request, the server's limit. */
const MAX_FACET_FIELDS = 20;

/** What every field of one batch shares: the list, its window, flags, filter and search. */
export type FacetShared = Omit<Schema<"FacetsRequest">, "fields">;

type Waiting = {
  field: Schema<"FacetFieldRequest">;
  resolve: (facet: Schema<"Facet">) => void;
  reject: (error: unknown) => void;
};

type Batch = { project: string; shared: FacetShared; waiting: Waiting[] };

const open = new Map<string, Batch>();

/**
 * One field's facet. Fields asked with the same project and shared part within
 * `FACET_BATCH_MS` go out as one request; each gets its own result or error back. A field's
 * error, or the whole request's, is thrown as the problem document, as `$api` queries throw it.
 */
export function askFacet(
  project: string,
  shared: FacetShared,
  field: Schema<"FacetFieldRequest">,
): Promise<Schema<"Facet">> {
  const key = JSON.stringify([project, shared]);
  return new Promise((resolve, reject) => {
    let batch = open.get(key);
    if (batch === undefined) {
      const created: Batch = { project, shared, waiting: [] };
      open.set(key, created);
      setTimeout(() => {
        // A full batch was already closed, and a newer one may hold the key.
        if (open.get(key) === created) {
          open.delete(key);
        }
        void send(created);
      }, FACET_BATCH_MS);
      batch = created;
    }
    batch.waiting.push({ field, resolve, reject });
    // The server takes at most 20 fields; the next ask starts a new batch.
    if (batch.waiting.length >= MAX_FACET_FIELDS) {
      open.delete(key);
    }
  });
}

async function send(batch: Batch): Promise<void> {
  // The same field asked twice (a `fetchQuery` racing a `useQuery`) is sent once.
  const fields: Schema<"FacetFieldRequest">[] = [];
  const slots = batch.waiting.map(({ field }) => {
    const text = JSON.stringify(field);
    const found = fields.findIndex((known) => JSON.stringify(known) === text);
    if (found !== -1) {
      return found;
    }
    fields.push(field);
    return fields.length - 1;
  });
  let results: Schema<"FacetResult">[];
  try {
    const { data, error } = await api.POST("/api/v1/projects/{project}/facets", {
      params: { path: { project: batch.project } },
      body: { ...batch.shared, fields },
    });
    if (error !== undefined || data === undefined) {
      throw error ?? new Error("facets: empty answer");
    }
    results = data.results;
  } catch (error) {
    for (const waiting of batch.waiting) {
      waiting.reject(error);
    }
    return;
  }
  for (const [index, waiting] of batch.waiting.entries()) {
    const result = results[slots[index] ?? -1];
    if (result?.facet !== undefined) {
      waiting.resolve(result.facet);
    } else {
      waiting.reject(result?.error ?? new Error("facets: missing result"));
    }
  }
}
