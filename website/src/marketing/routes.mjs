// The marketing pages (design §5) and the launch switch (design §10 M7).
// check-marketing.mjs and the layout read this file; each milestone adds its routes.

/** True since M7: marketing pages are indexable and / is the homepage. */
export const LAUNCHED = true;

/** Routes that always carry `noindex`, launched or not. */
export const ALWAYS_NOINDEX = ["/styleguide/"];

/**
 * Every marketing route, with a trailing slash. `/docs/` pages are not listed: Starlight owns them.
 * The compare pages are off for now (the user, 2026-11-26): their sources are in src/pages/_compare/, which
 * Astro does not build; moving the folder back and listing its routes here turns them on again.
 */
export const MARKETING_ROUTES = [
  "/",
  "/styleguide/",
  "/pricing/",
  "/self-hosting/",
  "/security/",
  "/integrations/",
];

/** @param {string} route */
export function isNoindex(route) {
  return !LAUNCHED || ALWAYS_NOINDEX.includes(route);
}
