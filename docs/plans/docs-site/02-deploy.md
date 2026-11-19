# W2 — Deploy to Cloudflare Pages, with PR previews

**Commit subject:** `Deploy the documentation site to Cloudflare Pages`

**Goal:** every push to `main` deploys the site to the Pages project `cetra-site` (production, `https://cetra.sh/` once the user attaches the domain), and every owner-authored PR deploys a preview at `https://pr-<number>.cetra-site.pages.dev/`. Headers, `robots.txt` and the "not released" banner are in place.

## Read first

- README § Decisions D-W-2, D-W-3, D-W-9, D-W-10; § Deployment; § What the user does by hand.
- `.github/workflows/ci.yml` (the `env.CI_CACHE_ALLOWED` expression and how jobs use `environment:`), `.github/workflows/packaging.yml` (a second workflow's shape), `.github/actions/js/action.yml`.
- `justfile` § `check-docs`.
- After install: `website/node_modules/wrangler/README.md` and `pnpm --dir website exec wrangler pages deploy --help`.

**Check first:** W1 is merged (`website/astro.config.mjs` exists). If not, stop.

## Files

| File | Change |
|---|---|
| `website/package.json`, `pnpm-lock.yaml` | `wrangler` 4.159.0 as a devDependency |
| `website/public/_headers` (new) | security and cache headers |
| `website/public/robots.txt` (new) | allow all, the sitemap |
| `website/astro.config.mjs` | the not-released banner via the page frontmatter default |
| `website/src/content/docs/docs/index.mdx` | the aside becomes the banner |
| `justfile` | `docs-deploy` |
| `.github/workflows/docs-deploy.yml` (new) | the deploy workflow |
| `website/README.md` | § Deployment |

## Steps

### 1. `wrangler`

Add `"wrangler": "4.159.0"` to `website/package.json` `devDependencies` and run `pnpm install`. If pnpm asks to allow a build script for `workerd` or `esbuild`, do **not** add it to `allowBuilds` first: run `pnpm --dir website exec wrangler pages deploy --help`. If that prints the help, the install script is not needed; leave `allowBuilds` alone. If it fails because a binary is missing, add `workerd: true` to `allowBuilds` in `pnpm-workspace.yaml` with a comment `# wrangler's runtime binary (D-W-3)`, and say so in the report.

### 2. `_headers` and `robots.txt`

`website/public/_headers`:

```
/*
  X-Content-Type-Options: nosniff
  Referrer-Policy: strict-origin-when-cross-origin
  X-Frame-Options: DENY
  Permissions-Policy: accelerometer=(), camera=(), geolocation=(), gyroscope=(), magnetometer=(), microphone=(), payment=(), usb=()

/_astro/*
  Cache-Control: public, max-age=31536000, immutable
```

(Pagefind's files are not content-hashed, so they keep the default caching.)

`website/public/robots.txt`:

```
User-agent: *
Allow: /

Sitemap: https://cetra.sh/sitemap-index.xml
```

Check after `astro build` that `website/dist/sitemap-index.xml` exists (Starlight writes it because `site` is set). If its name differs, use the real name in `robots.txt`.

### 3. The banner

Starlight shows a `banner: { content }` from a page's frontmatter. To put it on every page without repeating it, extend the schema default in `website/src/content.config.ts`:

```ts
import { defineCollection, z } from "astro:content";
import { docsLoader } from "@astrojs/starlight/loaders";
import { docsSchema } from "@astrojs/starlight/schema";

// Until the first release every page says so (D-W-9). The release that
// publishes 0.1.0 deletes this default (website/README.md § Releases).
const NOT_RELEASED = {
  content: "cetra is not released yet. These docs describe the development version.",
};

export const collections = {
  docs: defineCollection({
    loader: docsLoader(),
    schema: docsSchema({
      extend: z.object({
        banner: z.object({ content: z.string() }).default(NOT_RELEASED),
      }),
    }),
  }),
};
```

If Starlight 0.42.10 refuses to override `banner` through `extend` (a build error about a duplicate key), instead add `banner:` with the same content to the frontmatter of every page, and add a rule to `check-terms.mjs` that fails a page without it (with a test). Report which way you took.

Remove the `:::note` aside from `index.mdx`; the banner replaces it.

### 4. `just docs-deploy`

After `docs-dev` in the `justfile`:

```make
# Build the site and upload it to Cloudflare Pages (D-W-3). `branch` is `main`
# (production) or `pr-<number>` (a preview). Needs CLOUDFLARE_API_TOKEN and
# CLOUDFLARE_ACCOUNT_ID in the environment.
docs-deploy branch: js-install
    pnpm --dir website run build
    pnpm --dir website exec wrangler pages deploy dist --project-name cetra-site --branch {{branch}} --commit-dirty=true
```

`--commit-dirty=true` silences wrangler's warning about the generated files. Check the flag names against `wrangler pages deploy --help` (4.159.0); if one differs, use the help's name.

### 5. `.github/workflows/docs-deploy.yml`

```yaml
name: Docs deploy

on:
  push:
    branches: [main]
  pull_request:

permissions:
  contents: read

concurrency:
  group: docs-deploy-${{ github.event.pull_request.number || github.ref }}
  cancel-in-progress: true

jobs:
  deploy:
    # The job runs the PR's code with a deploy token: only main and owner-authored,
    # owner-triggered same-repository PRs (the CI_CACHE_ALLOWED gate of ci.yml, D-W-3).
    if: >-
      ${{
        (github.event_name == 'push' && github.ref == 'refs/heads/main') ||
        (github.event_name == 'pull_request' &&
         github.event.pull_request.user.login == 'sebastiencs' &&
         github.event.pull_request.head.repo.full_name == github.repository &&
         github.actor == 'sebastiencs' && github.triggering_actor == 'sebastiencs')
      }}
    runs-on: ubuntu-latest
    timeout-minutes: 15
    environment:
      name: ${{ github.event_name == 'push' && 'docs-production' || 'docs-preview' }}
      url: ${{ github.event_name == 'push' && 'https://cetra.sh/docs/' || format('https://pr-{0}.cetra-site.pages.dev/docs/', github.event.pull_request.number) }}
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - uses: ./.github/actions/js # zizmor: ignore[self-repository] actionlint 1.7.12 rejects the $/ form
      - name: Deploy
        env:
          CLOUDFLARE_API_TOKEN: ${{ secrets.CLOUDFLARE_API_TOKEN }}
          CLOUDFLARE_ACCOUNT_ID: ${{ secrets.CLOUDFLARE_ACCOUNT_ID }}
          BRANCH: ${{ github.event_name == 'push' && 'main' || format('pr-{0}', github.event.pull_request.number) }}
        run: just docs-deploy "$BRANCH"
      - name: Summary
        env:
          URL: ${{ github.event_name == 'push' && 'https://cetra.sh/docs/' || format('https://pr-{0}.cetra-site.pages.dev/docs/', github.event.pull_request.number) }}
        run: echo "Deployed to $URL" >> "$GITHUB_STEP_SUMMARY"
```

Run `just check-workflows`. Fix what `actionlint` and `zizmor` report; never silence a zizmor finding without a comment that says why, as the existing `ignore[self-repository]` comments do. If zizmor flags the secrets in a `pull_request` workflow, the owner gate is the answer: add `# zizmor: ignore[<rule>] owner-gated, same-repository PRs only (D-W-3)` on the flagged line.

### 6. The user's steps

Stop here and ask the user to do README § What the user does by hand, items 1 and 2 (Pages project, token, GitHub environments, then the custom domain and Web Analytics after the first production deploy). The workflow cannot succeed before item 1. Do not create tokens or projects yourself.

### 7. `website/README.md` § Deployment

Describe: production on push to `main`, previews per PR at `pr-<number>.cetra-site.pages.dev` (owner PRs only), `just docs-deploy <branch>` for a manual deploy with the two variables set, the `docs-production` and `docs-preview` environments, and that previews are not indexed (Pages adds `X-Robots-Tag: noindex` to them).

## Tests

- `just check-docs` passes; `website/dist/_headers` and `website/dist/robots.txt` exist after the build.
- `just check-workflows` passes.
- After the user's item 1: open a PR, see the `Docs deploy` run succeed and the preview URL serve `/docs/` with the banner. `curl -sI https://pr-<n>.cetra-site.pages.dev/docs/` shows `x-robots-tag: noindex` and the `_headers` values.
- After the merge: `https://cetra-site.pages.dev/docs/` serves the site; after item 2, `https://cetra.sh/` serves the landing page and `https://cetra.sh/docs/` the overview.

## Done when

- The workflow and recipe are committed and `just check` passes.
- A preview deploy has run green on this phase's own PR (or, if the user has not done item 1 yet, the report says so and the PR waits).
