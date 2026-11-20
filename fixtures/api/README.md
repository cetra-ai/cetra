# API example project

Synthetic data for UI mocks, the OpenAPI examples, and later storage and API tests. Nothing here is a real customer, key, or address. The emails are `ada@example.com`, `bob@example.com`, `guest@example.com` for the invite examples, and `new@example.com` for the admin-created account.

## Layout

| Path | Written by |
|---|---|
| `dataset/spans.json`, `dataset/scores.json` | hand |
| `static/*.json` | hand, except `static/sql-result.json` |
| `media/<sha256>.png` | hand |
| `responses/` | `just api-examples`: the real read handlers over `dataset/` |
| `manifest.json` | `just api-examples` |
| `static/sql-result.json` | `just api-examples`: the real `run_sql` handler's answer to `requests/sql-query.json` |
| `static/evaluator-stats.json` | hand-made from a draft by a throw-away test, so that the file is canonical JSON (the test was removed) |
| `static/online-rule-stats.json` | hand, canonical JSON: the hourly stats of online rules `…0700` (numeric scores, jobs in a few hours) and `…0701` (failed jobs, no scores) over `2025-12-31T12:00Z` to `2026-01-01T11:30Z` |
| `requests/*.json` | hand | example request bodies, checked against their types and `openapi.json` |

Do not edit `responses/`, `manifest.json` or `static/sql-result.json` by hand. Regenerate them with `just api-examples`.

`get_schema` is checked against `static/schema.json`, which A2's catalog test also reads. `get_metrics` is generated over `[2026-01-01T10:00:00Z, 11:00:00Z)` with `bucket=10m`. Its manifest query is empty, so the mocks serve it for any range.

`dataset/spans.json` is a JSON array of spans in the same shape as `fixtures/otlp/*/expected.json`, sorted by `(trace_id, start_time, span_id)`. `dataset/scores.json` is a JSON array of scores. Both use RFC 3339 timestamps with nine fractional digits.

## Fixed identifiers

| Thing | Value |
|---|---|
| Org | `01900000-0000-7000-8000-000000000010`, slug `acme`, name `Acme` |
| Project | `01900000-0000-7000-8000-000000000001`, slug `support-bot`, name `Support bot`, retention 30 days |
| Second project | `01900000-0000-7000-8000-000000000002`, slug `playground`, name `Playground` |
| User | `01900000-0000-7000-8000-000000000100`, `ada@example.com`, `Ada Example`, instance admin, org owner |
| Member | `01900000-0000-7000-8000-000000000101`, `bob@example.com`, `Bob Example`, org member. Used by the member examples |
| API key | `01900000-0000-7000-8000-000000000200`, prefix `sk-cetra-EXAMPL`. `api-key-created.json` uses the same synthetic `sk-cetra-EXAMPLE0…` secret as `setup-response.json`. It is not a live key |
| Personal token | `01900000-0000-7000-8000-000000000201`, prefix `pat-cetra-EXAMPL`. `personal-token-created.json` uses the synthetic `pat-cetra-EXAMPLE0…` secret. It is not a live token |
| Invitation | `01900000-0000-7000-8000-000000000300` for `guest@example.com`. `invite-created.json` uses the synthetic accept token `EXAMPLE0EXAMPLE0EXAMPLE0EXAMPLE0EXAMPLE0EXA`. It is not a live invitation |
| LLM connection | `01900000-0000-7000-8000-000000000400`, `OpenAI`. `requests/llm-connection-create.json` holds a synthetic key; it is not a live key |
| Deletions | `01900000-0000-7000-8000-000000000900` (`trace_ids`), `…0901` (`filter`), `…0902` (`user`), `…0903` (`project`), `…0904` (`org`), `…0905` (`user`, `resolving`, the `202` answer of `create_deletion`). `deletions.json`, `deletion.json`, `deletion-project.json`, `deletion-org.json` and `deletion-resolving.json` use them. They are not live deletions |
| Evaluator | `01900000-0000-7000-8000-000000000500`, `Relevance`, an LLM judge on connection `…0400`, score `relevance`. `static/evaluator-versions.json` is its version history: one item, version 1, equal to `evaluator.json` | `static/evaluator-stats.json` covers `…0500`, `…0501` and `…0502`.
| Check evaluator | `01900000-0000-7000-8000-000000000501`, `Answer is JSON`, a `json_valid` check, score `answer_is_json` (boolean). `static/evaluators.json` uses it |
| Webhook evaluator | `01900000-0000-7000-8000-000000000502`, `Toxicity webhook`, a webhook on host `moderation.example.com`, score `toxicity` (numeric), with a signing secret. `static/evaluators.json` uses it |
| Score definition | `01900000-0000-7000-8000-000000000600`, `relevance`, numeric from 0 to 1 |
| Online rule | `01900000-0000-7000-8000-000000000700`, `Relevance on answers`, a trace rule on evaluator `…0500`, filter `status = 'ok'`, budget 5 USD a day, enabled. `online-rule.json`, `online-rules.json` and `requests/online-rule-create.json` use it |
| Second online rule | `01900000-0000-7000-8000-000000000701`, `Relevance on spans`, a span rule on `…0500`, enabled and paused (`errors`). `online-rules.json` uses it; `online-rule-0701.json` is its detail |
| Eval jobs | `01900000-0000-7000-8000-000000000800` (done rule job of `…0700`, judge trace `e1000000000000000000000000000002`, score `…0900`), `…0801` (failed rule job, `provider status 503`), `…0802` (queued manual run of `…0500`). `eval-job.json`, `eval-jobs.json` and `eval-job-queued.json` use them. They are not live jobs |
| Backfills | `01900000-0000-7000-8000-000000000b00` (`done`), `…0b01` (`failed`, `interrupted`), `…0b02` (`running`, the `202` answer of `post_backfill`), all of rule `…0700`. `backfills.json`, `backfill.json` and the requests use them. They are not live backfills |
| Dashboard | `01900000-0000-7000-8000-000000000f01`, `Support overview`, version 3, four widgets (a heading, a line chart with a breakdown, a number and a table). `dashboards.json`, `dashboard.json` (list, get, create, update) and `dashboard-stale.json` (the 409 of a stale `PATCH`) use it. They are not live dashboards |
| Overview | `01900000-0000-7000-8000-000000000f02`, `Overview`, version 3, the first two widgets of `…0f01` (the heading and the line chart). `dashboard-overview.json` is the project's stored Overview (get, save) and is not a live dashboard |
| Annotation queue | `01900000-0000-7000-8000-000000000a01`, `Relevance review`, version 2, one score definition (`…0600`), 17 items (12 pending, 4 completed, 1 skipped). `annotation-queues.json`, `annotation-queue.json` and `requests/annotation-queue-create.json` use it. They are not live queues |
| Annotation queue items | `01900000-0000-7000-8000-000000000a11` (pending, leased by Ada, trace `a100…0001`) and `…0a12` (completed, trace `…0002`). `annotation-queue-items.json` and `annotation-queue-item.json` use them. `annotation-queue-items-added.json` is the `201` of an add of `requests/annotation-queue-items-add.json` (trace `…0003` is not found). They are not live items |
| Annotation queue by filter | `requests/annotation-queue-matching.json` is the body of a preview and an add by filter of queue `…0a01` (a trace filter over 2026-01-01 to 2026-01-02). `annotation-queue-matching-preview.json` is the `200` of the preview (3 matched) and `annotation-queue-matching-added.json` the `201` of the add (2 added, 1 existing). They are not live counts |
| Dataset | `01900000-0000-7000-8000-000000000c01`, `faq`, at version 2 with 3 live items. `datasets.json` and `datasets/…c01.json` use it. `requests/dataset-create.json` has the same name and schema |
| Dataset items | `01900000-0000-7000-8000-000000000d01`, `…0d02`, `…0d03`, and `…0d04` (added from a trace in version 4, `static/dataset-item-from-trace.json`) |
| Dataset revisions | `01900000-0000-7000-8000-000000000d11` (item `…0d01`, version 1), `…0d12` (`…0d02`, version 1, closed at version 2), `…0d13` (`…0d03`, version 1), `…0d14` (`…0d02`, version 2), `…0d15` (`…0d04`, version 4, from trace `a100…0001`) |
| Experiments | `01900000-0000-7000-8000-000000000e01` (`baseline`, completed, pinned to `faq` version 2), `…0e02` (`candidate`, completed in `experiments.json`, running in `experiment-created.json`). `experiment.json`, `experiments.json`, `experiment-created.json`, `experiment-results.json`, `experiment-comparison.json` and `requests/experiment-results.json` use them. They are not live experiments | `static/evaluator-stats.json` uses `…0e01` and `…0e02` as experiments with the relevance score.
| Model prices | `01900000-0000-7000-8000-000000000c00`, an org row `Llama 3.1 8B (local)` for `llama3\.1:8b`, any provider, 0 USD; `…0c01`, a project row `gpt-4o (negotiated)` for `gpt\-4o`, provider `openai`, from `2026-01-01T00:00:00.000000000Z`, input 2.0, output 8.0, cache read 1.0. `model-prices.json`, `model-price.json` and `model-price-resolution.json` use them, and `requests/model-price-create.json` is the body of the create |
| Prompts | `01900000-0000-7000-8000-000000001001`, `support-reply` (chat, versions 1 and 2; `production` on 1, `staging` on 2), and `01900000-0000-7000-8000-000000001002`, `summarize` (text, version 1, `production`). `prompts.json`, `prompts/…` and the `post_prompt` and `post_prompt_version` answers use them. `requests/prompt-create.json` and `requests/prompt-version-create.json` are the bodies of those two creates. They are not live prompts |
| Judge trace (test result example) | `e1000000000000000000000000000001`; it is not in the dataset |
| Created account | `01900000-0000-7000-8000-000000000102`, `new@example.com`. `admin-user-created.json` reuses that synthetic token in the reset link. It is not a live reset |
| Trace ids | `a1000000000000000000000000000001` through `…0006` |
| Span ids | `b1000000000000` plus the trace digit and the span digit. Trace 1 span 2 is `b100000000000012` |
| Missing parent of trace 6 | `b1000000000000ff` |
| PNG | `497790947d4666760ce38f3c00e852c71fdb66cae849bae8e9ede352719e1581` |

Session and user ids match `[a-z0-9-]+` so they can be file names. Trace 5 uses environment `cetra-eval` and is left out of the default lists, as is any trace in `cetra-playground`.

## Manifest

`manifest.json` is an array sorted by `(operation_id, file)`. Each entry names one example response:

```json
{
  "operation_id": "get_trace",
  "path_params": {"trace_id": "a1000000000000000000000000000001"},
  "query": {},
  "status": 200,
  "content_type": "application/json",
  "file": "responses/traces/a1000000000000000000000000000001.json"
}
```

`project` is never in `path_params`. The web mocks read `manifest.json`, match any `project`, and pick the query entry with the largest matching subset. `get_media` points at `media/<sha256>.png` with content type `image/png`.

`just check` runs the checks that remain in `crates/server/tests/api_examples.rs`: each static file and generated response round-trips through its Rust type, the responses match the OpenAPI schemas, the manifest covers every read operation and names only declared paths and statuses, the dataset is consistent (spans sorted, span ids unique within a trace, every score target exists, every `media_refs` entry matches the SHA-256 of `media/<sha>.png`, session and user ids stay `[a-z0-9-]+`), every file is used, and the scenario assertions hold. `just check` also runs the generator's up-to-date test in `crates/server/src/api_examples.rs`, which fails when a file differs from what the handlers return.
