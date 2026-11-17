# website

The cetra website (D-W-1): the landing page at `https://cetra.sh/` and the user documentation at `https://cetra.sh/docs/`. It is an Astro project with the Starlight theme, a pnpm workspace package (`@cetra/website`). The plan is in `docs/plans/docs-site/`.

Read `STYLE.md` before writing.

## Where pages live

- `src/pages/index.astro` is the landing page.
- Documentation pages are `.md` or `.mdx` files under `src/content/docs/docs/`. The URL is the path: `src/content/docs/docs/tracing/sessions.mdx` is `/docs/tracing/sessions/`, and `index.mdx` is the directory's page.

## Commands

Run them from the repository root.

```bash
just docs-dev
```

Serves the site on http://localhost:4321 (search works only in a build).

```bash
just check-docs
```

Lints, tests the scripts, type-checks, checks terms and descriptions, and builds the site with internal links validated.

## Add a page

1. Create the file under `src/content/docs/docs/` with `title` and a one-sentence `description` (under 160 characters) in the frontmatter.
2. Add it to the topic's `items` in `astro.config.mjs`.
3. Link to other pages with absolute paths and a trailing slash, for example `/docs/tracing/sessions/`. The build fails on a broken link or anchor, and on a relative link.
4. Run `just check-docs`.
