# W9 — Evaluation

**Commit subject:** `Document scores, evaluators, online rules, datasets and experiments`

**Goal:** the Docs topic's Evaluation group: eight pages (README § Page list › Evaluation) that take a user from writing a score to running online LLM judges and comparing experiments.

## Read first

- README § Page list › Evaluation; § Style guide; D-W-6, D-W-14.
- `website/STYLE.md`.
- `docs/spec/06-evaluation.md` (all of it), `docs/spec/09-api.md` § scores, evaluation, datasets, experiments, `docs/spec/11-ui.md` § the evaluator, online-rule, dataset and experiment pages, `docs/spec/04-metadata-db.md` § LLM connections (only what a user sees: which providers, what is stored encrypted), `docs/spec/08-auth.md` § egress.
- `sdks/python/README.md` § Scores, § Datasets and experiments; `sdks/typescript/README.md` § Scores, § Datasets and experiments, § Browser feedback.
- `docs/mvp-decisions.md` § Track I and § Track J, only to understand behaviour (never cite them).

**Check first:** `sdks/python/README.md` has the headings `## Scores` and `### Experiments`. If not, stop.

## Files

| File | Change |
|---|---|
| `website/src/content/docs/docs/evaluation/{index,scores,annotation,evaluators,llm-connections,online-rules,datasets,experiments}.mdx` (new) | the eight pages |
| `sdks/typescript/src/examples.test-helpers.ts`, `examples.test.ts` | new examples, if a page needs one the README lacks |
| `website/astro.config.mjs`, `index.mdx`, the tracing pages that mention scores | sidebar, cards, links |

## Steps

### 1. The pages

Required sections per page (sources in README § Page list). Every Python and TypeScript block tested (W3 markers); API calls as `bash` `curl` blocks.

- **`evaluation/index.mdx`** "Evaluation": the five building blocks and a decision table "I want to … → use …" (grade every production trace automatically → online rule; check a change before shipping → experiment; collect user feedback → scores from the browser; review traces by hand → annotation).
- **`evaluation/scores.mdx`** "Scores": what a score is (name, value types: numeric, boolean, categorical; target: trace, span or session; source: SDK, API, annotation, evaluator); writing one from Python and TypeScript (README `score` examples, tested; the Python one is `readme-scores`, reuse its text with `{/* test */}`), from the API (`curl`), from a browser with a `pk-cetra-` key (README § Browser feedback); updating a score by id; score definitions (allowed names and values per project); where scores show and how to filter on them.
- **`evaluation/annotation.mdx`** "Human annotation": the annotation form on a trace, which score definitions appear, who can annotate (roles), editing and ownership of a human score.
- **`evaluation/evaluators.mdx`** "Evaluators and LLM judges": kinds (checks, LLM judges, webhooks) with one paragraph each; variable mapping from span fields; templates; the judge's model and connection; test runs on a sample trace; the judge's own trace (environment `cetra-eval`, why it does not count towards your app's metrics); cost of judges.
- **`evaluation/llm-connections.mdx`** "LLM connections": which providers, adding one in settings, how keys are stored (encrypted with the instance key; self-hosted operators: link `/docs/self-hosting/configuration/` § Secrets), private-network egress and `security.allow_private_network_egress`.
- **`evaluation/online-rules.mdx`** "Online rules": rule = evaluator + filter + sampling + trigger; trace completion; sampling rate; cost caps and what happens when one is reached; preview; backfill over past traces; running one evaluator on one trace; plan limits on active rules (link plans, W10; until then none).
- **`evaluation/datasets.mdx`** "Datasets": items (input, expected output, metadata), versions and what changes a version, CSV and JSONL import (the column rules), export, adding a trace or span as an item, the dataset schema check, Python and TypeScript `Client` dataset calls (tested if the README has them as tested blocks; otherwise mark `untested: needs a running server` and say so in the report).
- **`evaluation/experiments.mdx`** "Experiments": `evaluate()` in both languages (task, scorers, concurrency), what gets recorded (one trace per item, scores, the experiment row), errors in a task or scorer, the results page and the compare view, running in CI (exit on a threshold from the returned summary; code untested).

### 2. Sidebar and links

Docs topic items, after Tracing:

```js
{
  label: "Evaluation",
  items: [
    { label: "Overview", link: "/docs/evaluation/" },
    { label: "Scores", link: "/docs/evaluation/scores/" },
    { label: "Human annotation", link: "/docs/evaluation/annotation/" },
    { label: "Evaluators and LLM judges", link: "/docs/evaluation/evaluators/" },
    { label: "LLM connections", link: "/docs/evaluation/llm-connections/" },
    { label: "Online rules", link: "/docs/evaluation/online-rules/" },
    { label: "Datasets", link: "/docs/evaluation/datasets/" },
    { label: "Experiments", link: "/docs/evaluation/experiments/" },
  ],
},
```

`index.mdx`: an "Evaluation" card. `tracing/traces.mdx` and `tracing/users.mdx`: link scores where they mention them. Integration pages' "Next": link evaluators.

## Tests

- `just check-py`, `just check-ts` (any new tested block), `just check-docs`.

## Done when

- `just check` passes; the eight pages are in the sidebar; every code block is marked.
