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

## Screenshots

The UI screenshots under `src/assets/screenshots/` are generated from the web UI running on its mocks, so they are the same on every run (D-W-12). `screenshots.json` lists them.

- Run `just docs-screenshots` in a pull request that changes a screen shown in the docs, and commit the new PNGs. A reviewer checks the PNG diff. Name shots to rewrite only those: `just docs-screenshots trace-view sessions`.
- The first time, install the browser: `pnpm --dir website exec playwright install chromium`. CI never downloads a browser.
- To add a shot, add an entry to `screenshots.json` (`name`, `path`, `waitFor`, and optionally `actions` and `waitAfter`), run the recipe, and show the PNG on its page with `<Image>`. `just check-docs` fails when a listed shot has no PNG or a PNG has no entry.

## Deployment

The site is a Cloudflare Pages project, `cetra-site` (D-W-3).

- A push to `main` deploys production: `https://cetra.sh/` (and `https://cetra-site.pages.dev/`).
- A pull request authored and triggered by the repository owner, from a branch of this repository, deploys a preview at `https://pr-<number>.cetra-site.pages.dev/`. Other PRs are not deployed, because the job runs the PR's code with a deploy token. Pages adds `X-Robots-Tag: noindex` to previews, so they are not indexed.
- The workflow is `.github/workflows/docs-deploy.yml`. It runs in the GitHub environments `docs-production` and `docs-preview`, which hold the secrets `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`.
- To deploy by hand, set those two variables and run `just docs-deploy <branch>`, where `<branch>` is `main` or `pr-<number>`.

`public/_headers` sets the security headers and long caching for `/_astro/*`; `public/robots.txt` allows all and points to the sitemap.
