// The marketing pages (design §5) and the launch switch (design §10 M7).
// check-marketing.mjs and the layout read this file; each milestone adds its routes.
import { COMPETITORS } from "./data/compare.mjs";

/** True since M7: marketing pages are indexable and / is the homepage. */
export const LAUNCHED = true;

/** Routes that always carry `noindex`, launched or not. */
export const ALWAYS_NOINDEX = ["/styleguide/"];

/** Every marketing route, with a trailing slash. `/docs/` pages are not listed: Starlight owns them. */
export const MARKETING_ROUTES = [
  "/",
  "/styleguide/",
  "/pricing/",
  "/self-hosting/",
  "/security/",
  "/integrations/",
  "/compare/",
  ...COMPETITORS.map((slug) => `/compare/${slug}/`),
];

/** @param {string} route */
export function isNoindex(route) {
  return !LAUNCHED || ALWAYS_NOINDEX.includes(route);
}
