# website

The cetra website (D-W-1): the landing page at `https://cetra.sh/` and the user documentation at `https://cetra.sh/docs/`. It is an Astro project with the Starlight theme, a pnpm workspace package (`@cetra/website`). The plan is in `docs/plans/docs-site/`.

Read `STYLE.md` before writing.

## Where pages live

- `src/pages/index.astro` is the landing page.
- Documentation pages are `.md` or `.mdx` files under `src/content/docs/docs/`. The URL is the path: `src/content/docs/docs/tracing/sessions.mdx` is `/docs/tracing/sessions/`, and `index.mdx` is the directory's page.

## Marketing pages

- Marketing pages are plain Astro pages under `src/pages/`, using `src/marketing/layouts/MarketingLayout.astro`. They never use Starlight. Their components, styles and constants are in `src/marketing/`.
- Every marketing route is listed in `src/marketing/routes.mjs`. `scripts/check-marketing.mjs` checks each one after the build: structure, metadata, `noindex`, internal links, and a 350 KB page-weight cap without fonts.
- `LAUNCHED` stays false until M7: every marketing page has `noindex` and `/` is the holding page.
- URLs and repeated strings live in `src/marketing/site.ts`.
- `/styleguide/` shows every component. Its source is `src/pages/styleguide.astro`.
- The design is `docs/website/00-design.md`.

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

The site is a Cloudflare Worker with static assets, `cetra-site`, configured in `wrangler.jsonc` (D-W-3). Its script answers only `/mcp`, the docs MCP server (D-AO-13); Cloudflare serves every other path from `dist/`.

- A push to `main` deploys production with `wrangler deploy`: `https://cetra.sh/` once the domain is attached, and `https://cetra-site.<subdomain>.workers.dev/`.
- A pull request authored and triggered by the repository owner, from a branch of this repository, deploys a Worker Preview named `pr-<number>` with `wrangler preview`, at `https://pr-<number>-cetra-site.<subdomain>.workers.dev/`. Other PRs are not deployed, because the job runs the PR's code with a deploy token. Cloudflare adds `X-Robots-Tag: noindex` to `workers.dev` Preview URLs, so they are not indexed.
- The workflow is `.github/workflows/docs-deploy.yml`. It runs in the GitHub environments `docs-production` and `docs-preview`, which hold the secrets `CLOUDFLARE_API_TOKEN` (Account › Workers Scripts › Edit) and `CLOUDFLARE_ACCOUNT_ID`.
- To deploy by hand, set those two variables and run `just docs-deploy <branch>`, where `<branch>` is `main` or `pr-<number>`.

`public/_headers` sets the security headers and long caching for `/_astro/*`; `public/robots.txt` allows all and points to the sitemap.

## Releases

At the first release, delete the `NOT_RELEASED` banner default in `src/content.config.ts`, and use `<Since v="x.y.z" />` for features added after it. `just release-prep` dates the changelog section that the badge compares against.

## Versioning

- The site documents one version, built from `main` (D-W-9).
- A feature merged since the latest release shows an "Unreleased" badge; the latest release is the first `## [x.y.z]` heading of `CHANGELOG.md`.
- Versioned snapshots start only at the first breaking major after 1.0.

## Generated files and checks

- `help/changelog.md` is generated from `CHANGELOG.md` at each dev start, build and check (`pnpm gen:changelog`); it is git-ignored.
- `llms.txt`, `llms-full.txt` and `llms-small.txt` are written by `starlight-llms-txt` at the site root.
- `just docs-links` checks external links with lychee 0.24.2. The `links` job of `nightly.yml` runs it every night and opens or updates the `nightly` issue on failure.
