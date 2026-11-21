# W13 — Help pages, `llms.txt`, the external link check; closeout

**Commit subject:** `Add the help pages and llms.txt, and close the docs track`

**Goal:** the Help group (FAQ, troubleshooting, changelog, support), the `<Since>` badge for the versioning policy, `llms.txt` for coding agents, a weekly external link check, and the track's closeout (roadmap, decisions, `AGENTS.md`, `website/README.md` § Releases).

## Read first

- README § Decisions D-W-9, D-W-11, D-W-13; § Page list › Help; § Versioning policy; § Analytics, SEO and agents.
- `CHANGELOG.md` (its section format: `## [Unreleased]`, `## [x.y.z] - YYYY-MM-DD`).
- `.github/workflows/nightly.yml` (the fuzz job and how it opens or updates the `nightly` issue).
- The SDK reference pages § Known limits (W11), `reference/api/` § Errors (W5), `self-hosting/monitoring.md`.
- After install: `website/node_modules/starlight-llms-txt/README.md`.

**Check first:** W1–W12 are merged (`website/src/content/docs/docs/reference/python/api.mdx` and `website/screenshots.json` exist). If not, stop.

## Files

| File | Change |
|---|---|
| `website/src/content/docs/docs/help/{faq,troubleshooting,support}.mdx` (new) | help pages |
| `website/scripts/changelog-page.mjs`, `changelog-page.test.mjs` (new) | `help/changelog` from `CHANGELOG.md` at build |
| `website/src/components/Since.astro` (new), `website/scripts/latest-release.mjs` (new, with test) | the badge |
| `website/package.json`, `pnpm-lock.yaml` | `starlight-llms-txt` 0.12.0; `gen:changelog` |
| `website/astro.config.mjs` | the plugin; the Help group |
| `.gitignore` | the generated changelog page |
| `.github/workflows/nightly.yml` | `links` job |
| `justfile` | `docs-links` |
| `website/README.md` | § Releases, § Versioning |
| `website/src/content/docs/docs/get-started/quickstart.mdx` | link troubleshooting |
| `docs/roadmap.md` | track W's row: done |
| `docs/mvp-decisions.md` | § Track W: anything decided during W1–W13 |
| `AGENTS.md` | the docs rules, final form |

## Steps

### 1. Help pages

- **`help/faq.mdx`** "FAQ": at most fifteen questions, each answered in two to four sentences with a link: Is cetra OpenTelemetry-compatible? Where is my data and in what format? Can I read the Parquet files without cetra? Does cetra run in the cloud? What is the licence? What needs a licence key? Which languages? Can I keep using the Langfuse or Laminar SDK? How do I delete a user's data? Does the SDK slow my app down (answer without numbers: batching, background export, no blocking on the network)? What happens if cetra is down (the SDK drops after its queue fills, nothing raises)? Use only facts from the specs and the SDK READMEs.
- **`help/troubleshooting.mdx`** "Troubleshooting": by symptom, each with "Check" and "Fix": no traces at all; traces without input or output; wrong nesting; missing cost; `401`, `403`, `413`, `429`, `402` on ingest (from `reference/otlp`); the SDK warns "no API key"; a short-lived script loses spans (flush); serverless; spans in the future or the past (clock skew); OTLP/gRPC through a proxy; the UI shows "not embedded" (a debug build); the setup link is lost (`docker logs`). Link each to the page that explains it.
- **`help/support.mdx`** "Support and security": GitHub issues (bugs), discussions (questions) if enabled, `hello@cetra.sh`, and how to report a vulnerability privately (GitHub's private vulnerability reporting on `cetra-ai/cetra`; if `SECURITY.md` exists at the repository root, link it; if not, say "email hello@cetra.sh with SECURITY in the subject" and note in the report that a `SECURITY.md` is missing).

### 2. The changelog page

`website/scripts/changelog-page.mjs` exports `changelogPage(markdown)` and a `main` that reads `../CHANGELOG.md` and writes `src/content/docs/docs/help/changelog.md`: frontmatter (`title: Changelog`, `description: Every user-visible change to cetra, release by release.`), an HTML "generated" comment, then the changelog without its `# Changelog` title and with relative links made absolute GitHub links. Add `website/src/content/docs/docs/help/changelog.md` to `.gitignore`, and `"gen:changelog": "node scripts/changelog-page.mjs"` to the scripts, run before `astro check`, `astro dev` and `astro build` like `gen:openapi`. The test: a small changelog in, the expected page out.

### 3. `<Since>`

`website/scripts/latest-release.mjs` exports `latestRelease(markdown)`: the version of the first `## [x.y.z]` heading of `CHANGELOG.md` that is not `[Unreleased]`, or `null`. Test it.

`website/src/components/Since.astro`:

```astro
---
// D-W-9: "Since x.y.z" for a released feature, "Unreleased" until then.
import { readFileSync } from "node:fs";
import { latestRelease } from "../../scripts/latest-release.mjs";

interface Props { v: string }
const { v } = Astro.props;
const latest = latestRelease(readFileSync(new URL("../../../CHANGELOG.md", import.meta.url), "utf8"));
const cmp = (a: string, b: string) => {
  const [x, y] = [a, b].map((s) => s.split(".").map(Number));
  for (let i = 0; i < 3; i++) if (x[i] !== y[i]) return x[i] - y[i];
  return 0;
};
const released = latest !== null && cmp(v, latest) <= 0;
---
<span class="since" data-released={released}>{released ? `Since ${v}` : "Unreleased"}</span>
<style>
  .since { font-size: 0.75rem; padding: 0.1rem 0.4rem; border-radius: 0.25rem; border: 1px solid var(--sl-color-gray-5); }
  .since[data-released="false"] { border-color: var(--sl-color-accent); color: var(--sl-color-accent); }
</style>
```

Fix the `CHANGELOG.md` path for the built file's location if `import.meta.url` points into `dist` at build time (read it from `process.cwd()` + `../CHANGELOG.md` instead; `astro build` runs in `website/`). Not used on any page yet: there is no release. Document it in `STYLE.md` § Versions.

### 4. `llms.txt`

Add `"starlight-llms-txt": "0.12.0"`; in `plugins`, `starlightLlmsTxt()` with the project name and a one-line description (check the option names in its README). After the build, `website/dist/llms.txt`, `llms-full.txt` and `llms-small.txt` exist. If the plugin writes them at `/docs/llms.txt` rather than `/llms.txt`, keep its default and say so in the report. Add a sentence to `platform/mcp.mdx` pointing coding agents at `https://cetra.sh/llms.txt`.

### 5. External links

`justfile`:

```make
# Check the docs site's external links (D-W-13); needs lychee 0.24.2 and a built site
docs-links:
    pnpm --dir website run build
    lychee --no-progress --max-concurrency 8 --accept 200,206,429 --exclude-path website/dist/docs/reference/api 'website/dist/**/*.html'
```

(The API pages hold `localhost` example URLs; also pass `--exclude '^http://localhost'`.)

`nightly.yml`: a `links` job modelled on the fuzz job: checkout, `./.github/actions/js`, `taiki-e/install-action` (same SHA as `ci.yml`) with `tool: lychee@0.24.2`, `just docs-links`, and on failure the same "open or update the `nightly` issue" step the fuzz job uses, with the title of the failing job. If `taiki-e/install-action` cannot install lychee, download the release tarball and check it against a pinned SHA-256, as `ci.yml` does for actionlint. Run `just check-workflows`.

### 6. Sidebar and links

Docs topic items, last:

```js
{
  label: "Help",
  items: [
    { label: "FAQ", link: "/docs/help/faq/" },
    { label: "Troubleshooting", link: "/docs/help/troubleshooting/" },
    { label: "Changelog", link: "/docs/help/changelog/" },
    { label: "Support and security", link: "/docs/help/support/" },
  ],
},
```

The quick start's "Nothing showing up?" links `/docs/help/troubleshooting/`.

### 7. Closeout

- `website/README.md` § Releases: at the first release, delete the `NOT_RELEASED` banner default (W2) and use `<Since>` from then on; § Versioning: D-W-9 in three lines.
- `AGENTS.md`: § Commands lists `check-docs`, `docs-dev`, `docs-gen`, `docs-deploy`, `docs-screenshots`, `docs-links`; § Tests names every docs check of README § Source of truth and drift (one bullet each, as for the other tests); § Commits and reports keeps the W1 rule.
- `docs/roadmap.md`: track W's row says **Done** with the PR numbers and `D-W-1`–`D-W-<n>`.
- `docs/mvp-decisions.md` § Track W: add any decision taken during W1–W13 (for example, a different plugin option or a dropped page), numbered from the next free `D-W-n`.
- Update this plan's README status line.

## Tests

- `changelog-page.test.mjs`, `latest-release.test.mjs`; `just check-docs`; `just check-workflows`.
- `just docs-links` passes locally (or lists only links the report explains).

## Done when

- `just check` passes; the four help pages are live in a preview; `llms.txt` is served; the `links` job has run once by `workflow_dispatch` and passed.
