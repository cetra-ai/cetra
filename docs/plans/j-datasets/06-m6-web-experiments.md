# J · M6 — Web: experiments, comparison, closeout

Implementation plan for milestone M6 of track J. It has 5 sub-phases, J6.1 to J6.5. **Haiku** implements each one in one go, with no context beyond this plan and the files it names. Sub-phases run in order, one commit each, and each ends with `just check` green. Sonnet reviews J6.1–J6.4 together, then J6.5.

M6 builds the experiment pages on M3's API: the experiments list (with the dataset filter and the 2–4 selection for compare), the experiment page (summary and results), the compare view (baseline first, deltas, per-item rows, regressions, an output diff), and the dataset page's Experiments tab. J6.5 closes the track: the end-to-end run, spec 11, the roadmap and the changelog. **J6.5 waits for M4 and M5 to be merged**; J6.1–J6.4 need only M3.

## Prerequisites

Check them before you start. If one fails, STOP and report.

- M3 is merged on `main`:

```bash
grep -n '"/api/v1/projects/{project}/experiments/compare"' crates/server/openapi.json
```

```bash
grep -n 'experiments' fixtures/api/manifest.json
```

- J6.2 needs M5's `web/src/routes/$org/$project/datasets/$datasetId.tsx` (the Experiments tab goes there). If M5 is not merged when J6.2 starts, do J6.2 without § 4 (the tab), and add § 4 at the start of J6.5.
- J6.5 only: M4 and M5 are merged (`grep -n "def evaluate" sdks/python/src/cetra/_experiments.py` prints a line; `web/src/components/datasets/import-dialog.tsx` exists).
- Node `26.13.0` at `$HOME/.local/share/nvm/v26.13.0/bin` (§ Commands).

## Read first (every sub-phase)

- `AGENTS.md`, all of it.
- `docs/plans/j-datasets/00-design.md`: § Decisions (D-J-10, D-J-11, D-J-13, D-J-14, D-J-17), § API contract (the `Experiment*`, `ScoreSummary`, `ResultRow`, `CompareRow` types), § Contract gaps, § Conventions.
- `docs/plans/j-datasets/05-m5-web-datasets.md` § Conventions for every sub-phase: M6 follows the same rules (query helpers, permissions, errors, tests, no new dependency).
- This file up to § J6.1, then your own sub-phase.
- `docs/spec/11-ui.md` § Experiments.

## Operation ids and fixtures

```bash
jq -r '.paths | to_entries[] | select(.key | contains("/experiments")) | .key as $p | .value | to_entries[] | "\(.key) \($p) \(.value.operationId)"' crates/server/openapi.json
```

```bash
grep -n '"file"' fixtures/api/manifest.json | grep -i experiment
```

M3's fixtures (hand-written, in `fixtures/api/static/`): `experiments.json` (a page with `…0e02` `candidate` then `…0e01` `baseline`, both on `faq` `…0c01` version 2), `experiment.json` (`…0e01`), `experiment-created.json`, `experiment-results.json` (results of `…0e01`, scores `exact` boolean and `quality` numeric) and `experiment-comparison.json` (`…0e01` vs `…0e02`, item `…0d02` regressed in the candidate). The manifest serves `list_experiments`, `get_experiment` (id `…0e01`), `get_experiment_results` (id `…0e01`) and `compare_experiments` (query `ids=…0e01,…0e02`). The other operation ids are `post_experiment`, `delete_experiment`, `post_experiment_results` and `post_experiment_finish` (M3 J3.4; the `jq` list is authoritative). In this plan `BASELINE_ID` is `01900000-0000-7000-8000-000000000e01` and `CANDIDATE_ID` is `01900000-0000-7000-8000-000000000e02`. Tests import the fixture files and assert on values read from them; the compare URL in tests is `/acme/support-bot/experiments/compare?ids=<BASELINE_ID>,<CANDIDATE_ID>`. If the manifest has no example for `compare` with exactly those ids, the compare tests answer it with `server.use` and the fixture file's body.

## Parallel work

J6.1–J6.4 run alongside M4 (only `sdks/`) and M5 (web). Shared with M5: `components/shell/nav.ts`, `mocks/writes.ts`, `mocks/writes.test.ts` (additive lines), and `routes/$org/$project/datasets/$datasetId.tsx` (J6.2 § 4 adds a tab). After a rebase on M5, keep both nav entries and both `writes.ts` blocks.

## Final layout after M6

```
web/src/api/queries-experiments.ts                        (J6.1)
web/src/lib/experiments.ts, experiments.test.ts           formatting, search parsers (J6.1)
web/src/lib/word-diff.ts, word-diff.test.ts               (J6.1)
web/src/components/shell/nav.ts                           + Experiments (J6.2)
web/src/mocks/writes.ts, writes.test.ts                   + the experiment writes (J6.1)
web/src/components/experiments/evaluate-snippet.tsx       (J6.1)
web/src/routes/$org/$project/experiments/index.tsx        list (J6.2)
web/src/components/experiments/experiments-table.tsx      list table, also on the dataset page (J6.2)
web/src/routes/$org/$project/datasets/$datasetId.tsx      + Experiments tab (J6.2)
web/src/routes/$org/$project/experiments/$experimentId.tsx   experiment page (J6.3)
web/src/components/experiments/score-summary.tsx          (J6.3)
web/src/components/experiments/results-table.tsx          (J6.3)
web/src/routes/$org/$project/experiments/compare.tsx      compare view (J6.4)
web/src/components/experiments/compare-table.tsx          (J6.4)
web/src/components/experiments/output-diff.tsx            (J6.4)
docs/spec/11-ui.md, docs/roadmap.md, CHANGELOG.md          (J6.5)
```

`experiments/compare.tsx` is a static segment; TanStack ranks it above `$experimentId`, so `/experiments/compare` never reaches the experiment page. J6.4's test checks it.

---

## J6.1 — Query helpers, formatting, word diff, write mocks, snippet

**Commit subject:** `web: add experiment helpers, word diff and mocks`

### Read first

- `web/src/api/queries-datasets.ts` (M5's J5.1; if M5 is not merged, `queries-online-rules.ts`).
- `web/src/lib/format.ts` (`formatDuration`, `formatCost`), `web/src/mocks/writes.ts`, `writes.test.ts`.
- M4's README sections for the snippet text: `grep -n "evaluate" sdks/python/README.md sdks/typescript/README.md` (if M4 is not merged, use § 6 below as is).

### Files

| File | Change |
|---|---|
| `web/src/api/queries-experiments.ts` | new |
| `web/src/lib/experiments.ts`, `experiments.test.ts` | new |
| `web/src/lib/word-diff.ts`, `word-diff.test.ts` | new |
| `web/src/mocks/writes.ts`, `writes.test.ts` | experiment writes |
| `web/src/components/experiments/evaluate-snippet.tsx`, `evaluate-snippet.test.tsx` | new |

### Steps

**1. `queries-experiments.ts`.**

```ts
export function experimentsInit(org: string, project: string, dataset?: string) {
  return {
    params: {
      path: { project: projectRef(org, project) },
      query: { limit: 100, ...(dataset ? { dataset } : {}) },
    },
  };
}
export function experimentsKey(org: string, project: string) { /* prefix, as onlineRulesKey */ }
export function experimentQuery(org: string, project: string, id: string) { /* GET …/experiments/{id} */ }
export function experimentResultsInit(org: string, project: string, id: string, errorsOnly: boolean) {
  return {
    params: {
      path: { project: projectRef(org, project), id },
      query: { limit: 100, ...(errorsOnly ? { errors_only: true } : {}) },
    },
  };
}
export function compareInit(
  org: string,
  project: string,
  ids: readonly string[],
  threshold: number,
  regressionsOnly: boolean,
) {
  return {
    params: {
      path: { project: projectRef(org, project) },
      query: {
        ids: ids.join(","),
        threshold,
        limit: 100,
        ...(regressionsOnly ? { regressions_only: true } : {}),
      },
    },
  };
}
```

M3 declares `ids` as one comma-separated string, `threshold` optional, `regressions_only` and `errors_only` booleans, and `limit`/`cursor` (`grep -n "experiments/compare" -A30 web/src/api/schema.gen.ts`). If the generated names differ, use them and report it.

**2. `lib/experiments.ts`.**

```ts
export const DEFAULT_THRESHOLD = 0.1;

/** `?dataset=` of the list: a UUID string, else undefined. */
export function parseExperimentsSearch(search: Record<string, unknown>): { dataset?: string };

/** `?errors=1` of the experiment page. */
export function parseExperimentSearch(search: Record<string, unknown>): { errors?: true };

/**
 * `?ids=a,b&threshold=0.2&regressions=1` of the compare view. `ids`: split on commas, trimmed,
 * empty parts dropped, duplicates dropped (first kept), UUID strings only, at most 4 kept.
 * `threshold`: a finite number in [0, 1000], else undefined (the view uses DEFAULT_THRESHOLD).
 */
export function parseCompareSearch(search: Record<string, unknown>): {
  ids: string[];
  threshold?: number;
  regressions?: true;
};

/** One line for a score summary (D-J-13):
 * numeric "mean 0.820 · median 0.900 · n 40";
 * boolean "pass rate 75 % · n 40" (pass_rate × 100, at most one decimal, no trailing ".0");
 * categorical "correct 30 · incorrect 10" (labels by count descending, then name; at most 4, then "· +N more");
 * text "n 40". Absent statistics are left out. */
export function scoreSummaryText(summary: Schema<"ScoreSummary">): string;

/** A delta as "+0.120" / "−0.050" (U+2212) / "0.000", three decimals; a boolean score's delta in points: "+12.5 pts". */
export function deltaText(delta: number, dataType: Schema<"ScoreSummary">["data_type"]): string;

/** "better" when delta > 0, "worse" when delta < 0, "same" at 0 (CG-J1: higher is better). */
export function deltaDirection(delta: number): "better" | "worse" | "same";

/** A score value of a result row (`ResultScore`: `data_type`, `value_num?`, `value_str?`): numeric with 3 decimals, boolean "pass" (1) / "fail" (0), categorical its label (`value_str`), text its first 40 characters with "…"; "—" when the value it needs is absent. */
export function scoreValueText(value: Schema<"ResultScore">): string;

/** Runs expected: item_count × repetitions. */
export function expectedRuns(experiment: Schema<"Experiment">): number;
```

Read the generated type names (`ScoreSummary`, `ResultScore` — M3 renamed the design's `ScoreValue` —, `Experiment`, `ExperimentSummary`, `ResultRow`, `CompareRow`, `CompareCell`, `ScoreDeltas`, `ExperimentComparison`) from `schema.gen.ts`; if M3 named one differently, use M3's name and report it.

**3. `lib/word-diff.ts`.** A word-level LCS diff, no dependency (D-J-17):

```ts
export type DiffPart = { kind: "same" | "added" | "removed"; text: string };

/** Above this many tokens on either side, `wordDiff` returns undefined (no diff shown). */
export const MAX_DIFF_WORDS = 2000;

/** Splits into tokens: each word with the whitespace after it, plus a leading whitespace run as its own token (`text.match(/^\s+|\S+\s*/g) ?? []`). Joining the tokens gives the text back. */
export function tokenize(text: string): string[];

/**
 * The parts that turn `before` into `after`: tokens in both (in LCS order) are "same", tokens
 * only in `before` "removed", tokens only in `after` "added". Adjacent parts of one kind are
 * merged. Removed parts come before added parts at each change. undefined when either side has
 * more than MAX_DIFF_WORDS non-whitespace tokens.
 */
export function wordDiff(before: string, after: string): DiffPart[] | undefined;
```

Implementation: tokens compare as exact strings. Trim the common prefix and suffix of the two token arrays first (they are "same"), then fill a classic O(n·m) LCS table over the middle in one `Uint16Array` of `(n+1)*(m+1)` cells (n, m ≤ 2,001 tokens, so at most ~4 M cells, 8 MB; an LCS length fits in 16 bits), then walk back from the end to build the parts. No recursion.

**4.** (No nav entry here: J6.2 adds it with the real list page, so no stub route is committed — AGENTS.md § No placeholders.)

**5. Write mocks.** In `writes.test.ts`, add `path.startsWith("/api/v1/projects/{project}/experiments") ||` to `covered`. In `writes.ts`, one `WRITE_OPERATIONS` entry and one `RESPONSES` entry per POST/DELETE under `/experiments` (from the `jq` list). Import M3's single-experiment fixture as `experimentDocument` (or `<list>.data[0]`). Answers: create → `201` `{ experiment: { ...experimentDocument, ...named(body) }, items: [], next_cursor: null }` (the flat `CreateExperimentResponse` of the design); results → `200` `{ stored: Array.isArray(body.results) ? body.results.length : 0, skipped: 0 }`; finish → `200` `{ ...experimentDocument, id: params.id, status: "completed" }`; delete → `noContent()`. Test `"deletes an experiment"`: `send("DELETE", `${PROJECT}/experiments/${BASELINE_ID}`)` answers 204.

**6. `EvaluateSnippet`.**

```ts
export function EvaluateSnippet(props: { datasetName?: string }): JSX.Element;
```

`Tabs` "Python" and "TypeScript", each a `<pre>` with a `CopyButton` (`components/copy-button.tsx`, label "code"). `name` is `props.datasetName ?? "my-dataset"`, inserted with `JSON.stringify(name)` so quotes are escaped. Python:

```python
import cetra

cetra.init()  # CETRA_API_KEY: a secret key with the ingest, read and write scopes

def task(input):
    return my_app(input)

def exact(output, expected):
    return output == expected

result = cetra.evaluate(dataset=<name>, task=task, scorers=[exact])
print(result.summary())
```

TypeScript:

```ts
import * as cetra from "@cetra-ai/sdk";

cetra.init(); // CETRA_API_KEY: a secret key with the ingest, read and write scopes

const result = await cetra.evaluate({
  dataset: <name>,
  task: async (input) => myApp(input),
  scorers: [function exact({ output, expected }) { return output === expected; }],
});
console.log(result.summary());
```

If M4's READMEs show a different call shape, use theirs and report it.

### Tests

`experiments.test.ts`:

1. `parseCompareSearch({ ids: "<a>, <b>,,<a>,x,<c>,<d>,<e>" })` → the first four distinct UUIDs `[a, b, c, d]`; `threshold: "0.2"` → 0.2; `"-1"`, `"abc"`, `"1001"` → undefined; `regressions: "1"` → true.
2. `scoreSummaryText`: numeric `{ data_type: "numeric", mean: 0.82, median: 0.9, count: 40, labels: {} }` → `mean 0.820 · median 0.900 · n 40`; boolean `pass_rate: 0.75` → `pass rate 75 % · n 40`; `pass_rate: 0.125` → `pass rate 12.5 % · n 8`; categorical with five labels → four plus `· +1 more`; text → `n 40`.
3. `deltaText(0.12, "numeric")` → `+0.120`; `deltaText(-0.05, "numeric")` → `−0.050`; `deltaText(0.125, "boolean")` → `+12.5 pts`; `deltaText(0, "numeric")` → `0.000`.
4. `scoreValueText` for each data type.

`word-diff.test.ts`:

1. Equal texts → one "same" part with the whole text.
2. `"the cat sat"` → `"the dog sat"`: `[same "the ", removed "cat ", added "dog ", same "sat"]`.
3. Joining the "same" and "removed" parts gives `before`; "same" and "added" gives `after` (check on three pairs, including empty strings).
4. 2,001 words on one side → `undefined`.
5. A 2,000-word text against itself with one word changed returns in under 1 s (`performance.now()` around the call) and has exactly one removed and one added part.

`evaluate-snippet.test.tsx`: with `datasetName='faq "v2"'` the Python block contains `dataset="faq \"v2\""`; the TypeScript tab shows `cetra.evaluate({`.

### Done when

`just check` passes.

---

## J6.2 — Experiments list, selection for compare, the dataset page's tab

**Commit subject:** `web: list experiments and pick them for comparison`

### Read first

- M5's `routes/$org/$project/datasets/index.tsx` (list page shape) and `$datasetId.tsx` (tabs, `parseDatasetSearch`).
- `web/src/components/ui/checkbox.tsx`, `select.tsx`, `badge.tsx`; `components/disabled-reason.tsx`; `components/time-text.tsx`.

### Files

| File | Change |
|---|---|
| `web/src/components/experiments/experiments-table.tsx` | new |
| `web/src/routes/$org/$project/experiments/index.tsx` | new: the list page |
| `web/src/components/shell/nav.ts` | Experiments entry |
| `web/src/routes/$org/$project/experiments/index.test.tsx` | new |
| `web/src/routes/$org/$project/datasets/$datasetId.tsx`, its test, `web/src/lib/datasets.ts`, its test | the Experiments tab |

### Steps

**1. `ExperimentsTable`.**

```ts
export function ExperimentsTable(props: {
  rows: Schema<"Experiment">[];
  /** Selected ids in the order they were picked; the first is the baseline. */
  selected: readonly string[];
  onSelectedChange: (ids: string[]) => void;
  /** Hide the Dataset column (the dataset page's tab). */
  hideDataset?: boolean;
}): JSX.Element;
```

Table `aria-label="Experiments"`. Columns: a `Checkbox` (`aria-label={"Select " + row.name}`); **Name** (link to `/$org/$project/experiments/$experimentId`); **Dataset** (`{dataset_name} v{dataset_version}`, linked to the dataset page when `dataset_id` is set, plain with a muted "(deleted)" after it otherwise); **Status** (`Badge`: "Running", "Completed", "Failed"; a failed one has the `error` in a `Tooltip`; a running one older than 24 h shows "Running · started {relative time}" — CG-J5); **Results** (`{result_count} / {expectedRuns(row)}`); **Created** (`TimeText`, relative). Checking a box appends the id to `selected`; unchecking removes it. A fifth check is refused: the box of every unchecked row is disabled while 4 are selected.

**2. The list page.** Route `createFileRoute("/$org/$project/experiments/")` with `validateSearch: parseExperimentsSearch`.

- `<h1>` "Experiments"; muted line "Runs of your app on a dataset, scored by the SDK. Pick two to four to compare."
- A dataset filter `Select` (`aria-label="Dataset"`): "All datasets" (removes `dataset`), then the project's datasets by name (`datasetsInit`, first 100). It writes `dataset` into the URL and clears the selection.
- `useInfiniteQuery` on `…/experiments` with `experimentsInit(org, project, search.dataset)`, paged with "Load more".
- Selection state lives in the page (`useState<string[]>`). Above the table: when 1 or more are selected, "Baseline: {name of selected[0]}" and the **Compare** button. Compare is enabled when 2–4 are selected and all share one `dataset_id` (not undefined); otherwise disabled with `DisabledReason`: fewer than 2 → "Pick 2 to 4 experiments"; different datasets → "Pick experiments of the same dataset". It navigates to `/$org/$project/experiments/compare` with `search={{ ids: selected.join(",") }}`.
- Each row also has a **Delete** action for members: a `DropdownMenu` on a "More" icon button (`aria-label={"Actions of " + row.name}`) with "Delete", opening a `ConfirmDialog` (title "Delete experiment", description "Deletes the experiment and its results. Its traces and scores stay in the project.", destructive, `confirmLabel` "Delete"). `DELETE …/experiments/{id}`; success invalidates `experimentsKey`; error `problemMessage(error, "The experiment could not be deleted")`. Below member the item is disabled (`DisabledReason` "Only members can delete experiments").
- Empty (no filter): `EmptyState` icon `FlaskConical`, title "No experiments", description "Run one from your code with evaluate():", followed by `<EvaluateSnippet />`. Empty with a dataset filter: the same with `datasetName` of that dataset.

**2b. Nav.** Read `web/src/components/shell/nav.ts` and any test that lists the nav labels. Import `FlaskConical` from `lucide-react` and append after Datasets (after Online rules if M5 is not merged): `{ to: "/$org/$project/experiments", label: "Experiments", icon: FlaskConical }`. Add "Experiments" to any test listing the nav labels. The route is the list page of § 2, in this same commit.

**3. Dataset page tab.** In `lib/datasets.ts`, add `"experiments"` to `DatasetTab` and to `parseDatasetSearch` (update its test). In `$datasetId.tsx`, add a tab **Experiments** after Versions: `useInfiniteQuery` with `experimentsInit(org, project, datasetId)`, an `ExperimentsTable` with `hideDataset`, the same Compare button and rules as the list page, and a link "Open in Experiments" to `/$org/$project/experiments?dataset=<id>`. Empty: `EvaluateSnippet` with the dataset's name. The query runs only while the tab is open (`enabled: tab === "experiments"`).

### Tests

`experiments/index.test.tsx`:

1. `"lists the experiments"` (fixture `static/experiments.json`): the table "Experiments" has a row per fixture experiment with its name linked to `/acme/support-bot/experiments/<id>`, `"{dataset_name} v{dataset_version}"`, the status label and `"{result_count} / {item_count × repetitions}"`.
2. `"filters by dataset"`: choose the fixture dataset in "Dataset"; the next list request has `dataset=<DATASET id>`; the URL has `dataset=`.
3. `"compares the selection with the first as baseline"`: check the candidate then the baseline; "Baseline: <candidate name>" shows; click Compare; the location is `/acme/support-bot/experiments/compare` with `ids=<CANDIDATE_ID>,<BASELINE_ID>` (the default handlers answer the compare page's requests, or install the fixture with `server.use`).
4. `"needs two experiments of one dataset"`: answer the list with two experiments of different `dataset_id`s; select both; Compare is disabled and its reason reads "Pick experiments of the same dataset".
5. `"allows at most four"`: answer the list with five experiments of one dataset; after checking four, the fifth checkbox is disabled.
6. `"shows a deleted dataset"`: an experiment without `dataset_id`; its row shows "(deleted)" and no dataset link.
7. `"shows the snippet when empty"`: an empty list; "No experiments" and `cetra.evaluate(` show.
8. `"deletes an experiment"`: open "Actions of <name>", Delete, confirm; the DELETE handler is called.

`$datasetId.test.tsx` (extend): `"lists the dataset's experiments"`: `?tab=experiments`; the list request has `dataset=<DATASET id>`; the table has no Dataset column header.

### Done when

`just check` passes.

---

## J6.3 — The experiment page

**Commit subject:** `web: show an experiment's scores and results`

### Read first

- D-J-13 (what the summary holds), D-J-14 (a hidden experiment is a 404).
- M5's `$datasetId.tsx` (loader with `notFound`), `components/datasets/items-table.tsx` (cell style, `previewJson`).
- `web/src/components/ui/switch.tsx`, `tooltip.tsx`; `web/src/lib/format.ts`.

### Files

| File | Change |
|---|---|
| `web/src/routes/$org/$project/experiments/$experimentId.tsx` | new |
| `web/src/routes/$org/$project/experiments/$experimentId.test.tsx` | new |
| `web/src/components/experiments/score-summary.tsx` | new |
| `web/src/components/experiments/results-table.tsx` | new |

### Steps

**1. Route.** `createFileRoute("/$org/$project/experiments/$experimentId")`, `validateSearch: parseExperimentSearch`, a loader that `ensureQueryData(experimentQuery(…))` and throws `notFound()` on 404 (which also covers an experiment outside the plan's data window, D-J-14).

**2. Data.** `useInfiniteQuery` on `…/experiments/{id}/results` with `experimentResultsInit(org, project, id, search.errors === true)`, paged with "Load more". The first page's `summary` is the summary (every page repeats it; read `pages[0].summary`). The experiment header reads `summary.experiment` once the results load, and `experimentQuery` before.

**3. Header.**

- A breadcrumb "Experiments" back to the list; `<h1>` the name; the description (muted) when set.
- A line: the dataset (`{dataset_name} v{dataset_version}`, linked to `/$org/$project/datasets/$datasetId?version=<dataset_version>` when `dataset_id` is set, "(deleted)" otherwise), the status badge (as J6.2), "Created" `TimeText`, "Finished" `TimeText` when `finished_at` is set, "{repetitions} repetitions" when above 1.
- When `status === "failed"`: an alert box "The run failed: {error}".
- When the metadata object is not empty: a collapsible "Metadata" (`details`/`summary`) with `JsonView`.

**4. `ScoreSummary` cards.**

```ts
export function ScoreSummaryCards(props: { summary: Schema<"ExperimentSummary"> }): JSX.Element;
```

A grid of small cards (`rounded-md border p-3`), `role="list"` with `aria-label="Summary"`, each `role="listitem"`:

- **Results**: `{result_count} of {expectedRuns}`;
- **Errors**: `error_count` (in `text-status-error` when > 0);
- **Latency**: `mean {formatDuration(latency_mean_ns)} · p95 {formatDuration(latency_p95_ns)}`, "—" when absent;
- **Cost**: `formatCost(cost_total)`, "—" when absent;
- one card per `scores[]` entry: the score name as the title and `scoreSummaryText(score)`.

**5. `ResultsTable`.**

```ts
export function ResultsTable(props: {
  rows: Schema<"ResultRow">[];
  scoreNames: string[];          // summary.scores names, in order
  repetitions: number;
}): JSX.Element;
```

Table `aria-label="Results"`, columns: **Item** (`shortId(dataset_item_id)` in a `Tooltip` with the full id), **Rep** (only when `repetitions > 1`; `repetition + 1`), **Input**, **Expected**, **Output** (each `previewJson`, `font-mono text-xs`, truncated with the full JSON in a `Tooltip` up to 2,000 characters), **Error** (a destructive `Badge` "Error" with the message in a `Tooltip`; empty otherwise), one column per score name (`scoreValueText` of `scores[name]`, "—" when absent; a boolean "fail" in `text-status-error`), **Latency** (`formatDuration(latency_ns)`), **Cost** (`formatCost(cost_total)`), **Trace** (a link "trace" to `/$org/$project/traces/$traceId` when `trace_id` is set; the trace may be past retention and answer 404, which the trace page already shows).

**6. Filters and empty states.** Above the table, a `Switch` labelled "Errors only" bound to `?errors=1`. When there are no rows: with "Errors only" → "No errors"; else when the experiment is running → "Waiting for results" and `EvaluateSnippet` (with the dataset name); else "No results".

### Tests

`$experimentId.test.tsx` (URL `/acme/support-bot/experiments/<BASELINE_ID>`):

1. `"shows the summary"` (fixtures `static/experiment.json`, `static/experiment-results.json`): the heading is the fixture name; the "Summary" list has a card per fixture score whose text equals `scoreSummaryText(…)` of the fixture's summary, and the Results card reads `"{result_count} of {expected}"`.
2. `"lists the results with their scores"`: the "Results" table has one row per fixture row, a column header per score name, and each row's trace link points to `/acme/support-bot/traces/<trace_id>`.
3. `"filters to errors"`: toggle "Errors only"; the URL has `errors=1` and the next results request has `errors_only=true`.
4. `"answers not found"`: the experiment request answers 404; the not-found view shows.
5. `"shows a failed run"`: answer the experiment and results with `status: "failed", error: "results upload failed"`; "The run failed: results upload failed" shows.
6. `"shows the snippet while waiting"`: a running experiment with no rows; "Waiting for results" and `cetra.evaluate(` show.
7. `"hides the repetition column for one repetition"`: no "Rep" header with `repetitions: 1`; with `repetitions: 3` it shows and the first row reads `1`.

### Done when

`just check` passes; in `just web-dev`, the example experiment's page shows its summary and results.

---

## J6.4 — The compare view

**Commit subject:** `web: compare two to four experiments`

### Read first

- D-J-13 (baseline, deltas, regression rule, warnings), CG-J1.
- J6.1's `lib/experiments.ts` and `lib/word-diff.ts`; J6.3's components.

### Files

| File | Change |
|---|---|
| `web/src/routes/$org/$project/experiments/compare.tsx` | new |
| `web/src/routes/$org/$project/experiments/compare.test.tsx` | new |
| `web/src/components/experiments/compare-table.tsx` | new |
| `web/src/components/experiments/output-diff.tsx`, `output-diff.test.tsx` | new |

### Steps

**1. Route.** `createFileRoute("/$org/$project/experiments/compare")`, `validateSearch: parseCompareSearch`. With fewer than 2 ids, render (no request) an `EmptyState` icon `FlaskConical`, title "Pick experiments to compare", description "Select two to four experiments of the same dataset on the Experiments page.", and a link "Go to Experiments".

**2. Data.** `useInfiniteQuery` on `…/experiments/compare` with `compareInit(org, project, ids, threshold ?? DEFAULT_THRESHOLD, regressions === true)`, paged with "Load more". `pages[0]` holds `experiments`, `deltas` and `warnings`. Errors: a `400` → an error box with `problemMessage(error, "These experiments cannot be compared")` (e.g. different datasets) and the link back; a `404` → "One of these experiments was not found. It may have been deleted or be outside your plan's data window." (D-J-14).

**3. Header.**

- `<h1>` "Compare experiments"; a breadcrumb back to the list.
- `warnings` (different dataset versions, or item sets that differ, so deltas cover only the items both ran without error) in an amber box, one line each (`role="status"`), the server's text as is.
- A summary table `aria-label="Scores"`: rows are score names (the union over `experiments[*].scores`, in the baseline's order then the others'), columns are the experiments (header: the name linked to its page, "Baseline" `Badge` on the first). Baseline cells show `scoreSummaryText`; candidate cells show it plus a delta `Badge` from the `deltas` entry whose `experiment_id` is that candidate (match by id, never by position): `scores[name]` is `{ delta, n_compared }`; the badge reads `deltaText(delta)`, colored by `deltaDirection(delta)` (`better` → `text-status-ok`, `worse` → `text-status-error`, `same` → muted), with `aria-label` `"{delta text} versus baseline"`, followed by a muted `n = {n_compared}` ("items both ran without error", D-J-13). A score absent from a candidate's `scores` shows no badge. Extra rows **Errors**, **Latency (mean)** and **Cost** use the summary fields, with no delta badge.
- Controls: a number `Input` `aria-label="Regression threshold"` (step 0.05, min 0, max 1000, bound to `?threshold`, written after 300 ms), with the help text "A score that drops by more than this, or a new error, marks a regression. Higher scores are better." (CG-J1); a `Switch` "Regressions only" bound to `?regressions=1`.

**4. `CompareTable`.**

```ts
export function CompareTable(props: {
  rows: Schema<"CompareRow">[];
  experiments: Schema<"ExperimentSummary">[];   // baseline first
}): JSX.Element;
```

Table `aria-label="Items"`: columns **Item**, **Input**, **Expected**, then one column per experiment (header the experiment name; the baseline's has "Baseline"). Each experiment cell (matched by `cells[].experiment_id`) shows, stacked:

- the output: for the baseline `previewJson(output, 300)`; for a candidate an `OutputDiff` against the baseline cell's output;
- an "Error" badge with the message in a tooltip when `error` is set;
- the score values as small chips `name value` (numbers with 3 decimals);
- latency (`formatDuration`), and a "trace" link when `trace_id` is set.

A candidate cell with `regressed: true` gets `bg-status-error/10` and a `Badge` "Regressed" (`aria-label="Regressed"`). A row with any regressed cell gets `data-regressed="true"` (for the test). A row holds cells only for the experiments that have a result for the item (D-J-13): look each column's cell up with `row.cells.find((cell) => cell.experiment_id === experiment.experiment.id)`, never by index; when there is none, the cell shows a muted "Not run".

**5. `OutputDiff`.**

```ts
export function OutputDiff(props: { before: unknown; after: unknown }): JSX.Element;
```

Each side becomes text: a string as itself, anything else `JSON.stringify(value, null, 2)`, `undefined` as `""`. When both texts are equal: the text in muted style with the label "same as baseline" (sr-only). Otherwise `wordDiff(before, after)`: `undefined` (too long) → the candidate's text cut to 300 characters and the note "Too long to diff"; otherwise the parts in a `<p className="whitespace-pre-wrap font-mono text-xs">`: "same" plain, "added" in `<ins className="bg-status-ok/15 no-underline">`, "removed" in `<del className="bg-status-error/15">`. Cut the rendered text at 2,000 characters with "…" (count across parts; stop adding parts past the limit).

### Tests

`output-diff.test.tsx`:

1. `before: "the cat sat", after: "the dog sat"` → an `ins` whose text is "dog " and a `del` whose text is "cat ".
2. Equal objects → no `ins`/`del`, the text shows.
3. A 2,001-word `after` → "Too long to diff".

`compare.test.tsx`:

1. `"shows the scores with deltas"` (fixture `static/experiment-comparison.json`, served for `ids=<BASELINE_ID>,<CANDIDATE_ID>`): the "Scores" table has a column per fixture experiment, the first headed with "Baseline"; for the candidate, each score's badge reads `deltaText(delta)` from the fixture's `deltas` (`scores[name].delta`), with `aria-label` "… versus baseline", and `n = <n_compared>` shows next to it.
2. `"highlights regressed items"`: in the fixture, item `…0d02`'s candidate cell has `regressed: true`; that row has `data-regressed="true"` and a "Regressed" badge.
3. `"passes the threshold and the regressions filter"`: type `0.25` in "Regression threshold" and toggle "Regressions only"; the next request has `threshold=0.25` and `regressions_only=true`; the URL has `threshold=0.25&regressions=1`.
4. `"shows the version warning"`: answer with `warnings: ["the experiments used different versions of the dataset"]`; the text shows in a status region.
4b. `"shows the item-set warning"`: answer with a second warning, the one the server sends when the item sets differ (copy its exact text from M3 J3.5); both lines show.
4c. `"matches cells by experiment id"`: answer a row whose `cells` hold only the candidate's cell (listed first); the baseline column shows "Not run" and the candidate column shows the candidate's output.
5. `"explains a refused comparison"`: answer 400 with detail "the experiments belong to different datasets"; that text shows with the link back.
6. `"asks for a selection without ids"`: `/acme/support-bot/experiments/compare`; "Pick experiments to compare" shows and no compare request is made (the unhandled-request check would catch one).
7. `"is not the experiment page"`: the compare URL renders "Compare experiments", not a not-found for an experiment named "compare".

### Done when

`just check` passes; in `just web-dev`, selecting the two example experiments on the list opens the compare view with deltas and the output diff.

---

## J6.5 — Closeout: end-to-end run, spec 11, roadmap, changelog

**Commit subject:** `docs: close track J`

### Read first

- M4's `docs/plans/j-datasets/04-m4-sdks.md` § J4.3 step 3 and § J4.6 step 3 (the two e2e tests that run an experiment against a real server).
- `docs/spec/11-ui.md` § Routes, § Traces list (the "Other bulk actions" sentence), § Trace view (the "Add to dataset … wait for their API contract" sentence), § Datasets, § Experiments.
- `docs/roadmap.md` J row; `CHANGELOG.md` § [Unreleased].
- `tools/e2e/run.sh`.

### Files

| File | Change |
|---|---|
| `web/src/routes/$org/$project/datasets/$datasetId.tsx` and its test | only if J6.2 § 4 was deferred (§ Prerequisites) |
| `docs/spec/11-ui.md` | the J pages as built |
| `docs/roadmap.md` | J row → Done |
| `CHANGELOG.md` | § [Unreleased] |

### Steps

1. **Deferred tab.** If J6.2 skipped the dataset page's Experiments tab, do J6.2 § 3 and its test now.
2. **End-to-end.** Check the two e2e tests exist: `grep -n "test_dataset_experiment_round_trip" sdks/python/tests/test_e2e.py` and `grep -n "runs an experiment and reads its results back" sdks/typescript/src/e2e.test.ts`. If either is missing, STOP and report. Run `just e2e` (it builds the server and runs both SDK suites against `cetra serve`). It must pass. If it fails in a J test, STOP and report the failure with the test name and the server log's relevant lines (`$dir/serve.log`, ids and status codes only).
3. **Spec 11.**
   - § Routes: replace the two J lines with
     `/:org/:project/datasets · /datasets/:datasetId   datasets (track J, D-J-17)` and
     `/:org/:project/experiments · /experiments/:experimentId · /experiments/compare?ids=   experiments (track J, D-J-13)`.
   - § Traces list: the sentence "Other bulk actions (add to dataset, export CSV) wait for their API contract …" becomes "There is no bulk add to dataset in the MVP (CG-J4); export CSV waits for its API contract (own plan; D123)." Keep the rest of the line.
   - § Trace view: "Add to dataset and run evaluator on this trace wait for their API contract (own plan; D123)." becomes "Actions: Run evaluator (track I) and Add to dataset (track J, D-J-8): one dialog for the trace root or the selected span, input and output prefilled and editable, into an existing or a new dataset."
   - § Datasets: replace the paragraph with what M5 built: list (search, New dataset with optional JSON Schemas); dataset page with version selector (past versions read-only), tabs Items (side sheet: JSON textareas, save, delete), Versions, Experiments, Settings (edit, schemas, delete by typing the name); Add item; Import wizard (file → preview and column mapping for CSV → import → row errors; 100 MiB, D-J-6); Export JSONL/CSV of the shown version (D-J-7); a per-item source trace link.
   - § Experiments: replace the whole paragraph (it drops the old "List grouped by dataset" and the experiment page's "regressions" filter, which M6 does not build: the page has "Errors only", and regressions live in the compare view) with what M6 built: list (dataset filter, select 2–4 of one dataset, the first is the baseline, delete); experiment page (summary cards: results, errors, latency, cost, one per score; results table with input, expected, output, error, scores, latency, cost, trace link; "Errors only"; the evaluate snippet while empty); compare view (score table with deltas against the baseline, colored by direction, higher is better (CG-J1); warnings; per-item rows with a word diff of outputs against the baseline, regressions highlighted, threshold and "Regressions only"). No server evaluators on experiments (D-J-11).
4. **Roadmap.** J row: Plan `[plans/j-datasets/](plans/j-datasets/): design doc, then one plan per milestone (M1–M6)`; Status **Done.** with one sentence per milestone and its PR numbers (`gh pr list --state merged --search "j-datasets OR datasets OR experiments" --json number,title` to find them; list the numbers you can confirm and say "PR numbers to confirm" for the rest); Decisions `D-J-1–D-J-19`. Update the **Order** paragraph only if it still says J is pending.
5. **Changelog.** Grep first (`grep -n "Datasets:\|Experiments:" CHANGELOG.md`) and add only what is missing. Under `### Added` in § [Unreleased], if M1–M5 did not already add them: "Datasets: versioned datasets of input, expected output and metadata items, with optional JSON Schemas, CSV and JSONL import, JSONL and CSV export, and Add to dataset from a trace or span." and "Experiments: `cetra.evaluate()` in the Python and TypeScript SDKs runs a task on a dataset and scores it; the experiment page and a comparison of 2–4 experiments with regressions highlighted; `dataset_items` and `experiment_items` in the SQL console." Add one line for this milestone if those lines exist: "Experiments: the experiments list, experiment page and compare view."

### Tests

No new test. `just check` and `just e2e` pass.

### Done when

`just check` and `just e2e` pass; spec 11, the roadmap and the changelog describe what shipped; the J row says Done.

---

## Review checklist (Sonnet)

After J6.4:

- The compare view sends the ids in selection order and labels the first as the baseline everywhere.
- Delta colors follow `deltaDirection` only; no page infers a score's direction (CG-J1).
- The word diff is bounded (2,000 words, 2,000 rendered characters) and has no recursion.
- 404s of hidden experiments (D-J-14) show the not-found view, not an error crash.
- Write controls check `canManageDatasets`.

After J6.5: the spec sentences match the pages; the roadmap row's PR numbers are real.

## Commands

```bash
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
```

```bash
just check
```

```bash
just e2e
```

Focused runs: `pnpm --dir web test -- experiments`, `pnpm --dir web test -- word-diff`, `pnpm --dir web run typecheck`.

## Report

When a sub-phase is done, report what was done, any deviation and why (an operation id, a type name, a parameter shape that differed from this plan), whether `just e2e` ran (J6.5), and anything left undone or unverified. M6 adds no dependency.
