// The docs search: pure, shared by the build (docs-index.mjs) and the Worker (docs-mcp.mjs).
// It imports only minisearch, because the Worker bundles this file (D-AO-13).
import MiniSearch from "minisearch";

/** The site origin. A deliberate copy of markdown.mjs's SITE: the Worker cannot import markdown.mjs (it imports tidymd). */
export const SITE = "https://cetra.sh";
/** Common words dropped from a query before searching, unless every term is one of them. */
export const STOPWORDS = new Set(
  "how do i can a an the to from of in on for with my me is are it and or what which send".split(
    " ",
  ),
);
/** The index options; loadJSON needs the same ones. A hit's `id` is the page path "/docs/<p>/". */
export const INDEX_OPTIONS = {
  idField: "url",
  fields: ["title", "headings", "body"],
  storeFields: ["title", "description"],
};
const STRICT = { prefix: true, fuzzy: 0.2, boost: { title: 3, headings: 2 }, combineWith: "AND" };
const LOOSE = { ...STRICT, combineWith: "OR" };

/** Hits: [{title, url: SITE + hit.id, snippet: hit.description}], best first, at most `limit`.
 *  The query's words minus STOPWORDS (all of them when every word is a stopword); all the
 *  remaining terms first (AND); when that gives fewer than `limit`, the OR hits not already
 *  listed follow, so a long question still finds the page. */
export function search(index, query, limit) {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  const kept = words.filter((word) => !STOPWORDS.has(word));
  const terms = (kept.length > 0 ? kept : words).join(" ");
  const hits = index.search(terms, STRICT).slice(0, limit);
  if (hits.length < limit) {
    const seen = new Set(hits.map((hit) => hit.id));
    for (const hit of index.search(terms, LOOSE)) {
      if (hits.length >= limit) break;
      if (!seen.has(hit.id)) hits.push(hit);
    }
  }
  return hits.map((hit) => ({ title: hit.title, url: SITE + hit.id, snippet: hit.description }));
}

export function loadIndex(json) {
  return MiniSearch.loadJSON(json, INDEX_OPTIONS);
}
