import { HttpResponse, http } from "msw";
import type { Schema } from "@/api/types";

/** The answer of one field, or undefined for an empty values facet. */
export type FacetAnswerer = (
  field: Schema<"FacetFieldRequest">,
  body: Schema<"FacetsRequest">,
) => Schema<"Facet"> | undefined;

const EMPTY: Schema<"Facet"> = { kind: "values", total: 0, missing: 0, values: [], more: false };

/** A `POST /facets` handler answering each field with `answer`. */
export function facetsHandler(answer: FacetAnswerer) {
  return http.post("*/api/v1/projects/:project/facets", async ({ request }) => {
    const body = (await request.json()) as Schema<"FacetsRequest">;
    return HttpResponse.json({
      view: body.view,
      results: body.fields.map((field) => ({
        field: field.field,
        facet: answer(field, body) ?? EMPTY,
      })),
    });
  });
}
