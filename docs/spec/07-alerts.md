# 07 — Alerts

Per-project alert rules over five metrics, evaluated in the background over sliding windows, with firing/resolved state and notifications to Slack, a signed webhook and email. Track K's plan (`docs/plans/k-alerts/`) and decisions D-K-1–D-K-16 give the details. Alerts are core (MIT) code (D-K-15).

## Rule model

`alert_rules` (04). The condition (`config`, JSON) is flat; fields that do not apply to the metric must be absent (D-K-2):

| `metric` | Fields | Computed over the window |
|---|---|---|
| `error_rate` | `filter?`, `window`, `comparator`, `threshold` (fraction 0–1), `min_count` (default 20) | traces with `error_count > 0` / traces |
| `cost` | `filter?`, `window`, `comparator`, `threshold` (USD ≥ 0) | `sum(cost_total)` on traces as `Decimal128(38, 18)` (0 when none; a non-finite cost fails the query, D-A2-4) |
| `latency_p95` | `filter?`, `window`, `target` (`trace` default, or `llm_span`), `comparator`, `threshold` (ms > 0; for `trace`, below the window length), `min_count` (default 20) | `approx_percentile_cont(duration_ns, 0.95)` in ms, of traces (by start time) or of `llm` spans (by end time, D-K-18) |
| `score` | `filter?`, `window`, `score_name`, `aggregation` (`avg` default, or `pass_rate`), `comparator`, `threshold` (`pass_rate`: 0–1), `min_count` (default 10) | `avg(value_num)` of live trace/span scores of that name created in the window, whose trace started at most 7 days before it (any end time, D-K-17); `pass_rate` keeps boolean scores only |
| `issue_occurrences` | `issue_id` or `source_id` (exactly one), `window`, `comparator`, `threshold` (a count, ≥ 0) | live `issue:<id>` occurrence scores created in the window, of the names resolved at each evaluation (below); no trace filter, no default exclusions, no `min_count` (D-IC-20) |

- `window` ∈ {`1m`, `5m`, `15m`, `1h`, `6h`, `24h`}; `comparator` ∈ {`gt`, `gte`, `lt`, `lte`}; `min_count` 1–1 000 000 (not for `issue_occurrences`).
- For `issue_occurrences`, `filter`, `min_count`, `target`, `score_name` and `aggregation` are refused (`400`, "does not apply to issue_occurrences"). Exactly one of `issue_id` and `source_id` is required, and `issue_id` and `source_id` are refused on every other metric. `threshold` is finite and at least 0. Zero is a value, never "no data": a count of 0 is read, so an `lt` rule can fire on it.
- The window of an `issue_occurrences` rule is `15m` or more. `1m` and `5m` are refused (`400` on `condition.window`: "issue occurrences need a window of 15m or more"): an occurrence is written after its trace completes, so a short window can miss it. Error issues are written within about a minute of the trace. The occurrences of an LLM issue (bad score, feedback, topic) are written when a candidate joins an issue, minutes after its score or trace, or by a refit, which runs every 6 hours; each is dated at the trace's start, so a refit can add occurrences up to about 6 hours in the past. Docs advise a `6h` or `24h` window for LLM sources.
- The issue or source must exist in the rule's project (`400` on `condition.issue_id` or `condition.source_id`).
- `filter` is a **`traces`** filter-DSL expression for every metric; span and score measures keep rows whose trace passes it (as dashboard queries, 05). Experiment traces are always excluded; evaluation traces (`environment = 'cetra-eval'`) are excluded unless the filter names `environment` (05 § Default exclusions).
- Rule fields: `name` (1–100, unique per project), `enabled`, `channel_ids` (≤ 10 channels of the same project), `cooldown_s` (300–604 800, default 3 600), `notify_while_firing` (default false).
- Limits: 100 rules and 20 channels per project (D-K-5). On the cloud Free plan an org may have one enabled rule; creating or re-enabling past the plan limit is `403 plan_limit` (D178, D-K-14). Self-hosted has no limit.
- There is no `group_by` in the MVP: a per-model cost alert is a rule with a filter (D-K-2).
- The web preview (the alert form and the rule list) draws the dashboard's trace error rate, cost, p95 latency and score average through the widget query (same source, time column and filter). It may therefore differ from the value the evaluator computes for score measures (7-day trace lookback, D-K-17) and for `llm_span` latency (end-time window, D-K-18); the web preview does not draw `llm_span` latency at all.

## Evaluation loop

- A tick every 60 s evaluates each enabled rule of an active project and org that was never evaluated or whose `last_evaluated_at + interval − 5 s ≤ now` (`interval` = 60 s for windows ≤ 15 m, 300 s otherwise) (D-K-3).
- Each evaluation is one aggregate query (`cetra_storage::alert_value`) over `[now − window, now)` (memtable included) through the query context every read uses, so the plan's data-access window applies (D175, D-K-16). At most 4 evaluations run at once, each with a 30 s timeout; a tick waits for the previous one.
- An evaluation is *insufficient* when the sample count is below `min_count`, *breaching* when it is not insufficient, has a value, and `value <comparator> threshold`, else *clear*. An `issue_occurrences` rule has no `min_count`, so its count is always a value and it is never insufficient.

```
ok / no_data ──(breaching)──► firing ──(2 clear in a row)──────────► ok       (resolved)
      │                          └──(2 insufficient in a row)──► no_data  (no_data)
      ├─(clear)────────► ok          (no notification)
      └─(insufficient)─► no_data     (no notification)

no_data after firing ──(breaching)──► firing (fired) · ──(clear)──► ok (resolved) · ──(insufficient)──► no_data (no event)
```

- `ok/no_data → firing` records a `fired` event; `firing → ok` a `resolved` event; `firing → no_data` a `no_data` event (D-K-20): a rule whose data stopped is not resolved, and the notification says "No data". A `no_data` rule that was firing resolves, or fires again, on the first reading with data (it is marked by `clear_streak = 1`). While firing, a `reminder` event is recorded every `cooldown_s` when `notify_while_firing` is on. Each event creates one pending delivery per channel, in the same transaction.
- A failed query changes no state; it sets `last_error` and is never treated as no data.
- **Issue names** (D-IC-20). Each `issue_occurrences` evaluation resolves its names before it reads. For an issue, the names are the issue, or its merge target when it is merged, and every issue merged into that target, each as `issue:<id>`. For a source, they are the source's issues that are neither `ignored` nor `merged`, and the issues merged into each. The names are counted in chunks of 1 000 (`MAX_COUNT_NAMES`), and the counts are summed, so a source has no limit on its issues. If the issue or the source no longer exists, the evaluation fails with `issue not found` or `issue source not found`, handled as a failed query is. Deleting a source leaves its rules in place.
- Changing a rule's condition, or disabling or re-enabling it, resets it to `ok` with no event (D-K-4). A write is fenced by `updated_at`, so an evaluation never overwrites a concurrent edit.

## Channels

`notification_channels` (04), per project; the whole config is one sealed JSON (`config_enc`, AAD `notification_channels:config_enc:<id>`). Responses never show a URL path, a secret or a header value; they show `target_hint` (D-K-6).

| Kind | Config | Delivery |
|---|---|---|
| `slack` | `{url}` (an `https` Slack incoming webhook) | Block Kit message: title, project, metric, value, threshold, window, samples, buttons to the rule and to the trace list |
| `webhook` | `{url, secret (16–256), headers?}` (≤ 10 headers) | `POST` JSON (below), `X-Cetra-Signature: t=<unix>,v1=<hex HMAC-SHA256(secret, "<t>.<body>")>` (as the webhook evaluator), `X-Cetra-Delivery: <delivery id>` |
| `email` | `{recipients}` (1–20) | the `alert` email template (08 § Email), one message per recipient; creating one needs SMTP, and existing ones show "SMTP not configured" without it. When `alerts.email_recipients` is `members` (12 § Configuration; unset, it is `members` under `auth.signup = open` and `any` otherwise) each recipient must be a verified member of the org (D-K-19, D-K-21): create and update refuse others with `400` on `recipients`, and a send, test sends included, skips them and fails the delivery; a channel saved under an earlier policy is checked again at each send. Each org may send 1,000 recipient messages per UTC day, test sends included (D-K-19) |

Webhook payload (`type` is `alert.fired`, `alert.resolved`, `alert.reminder` or `alert.test`; `value` and `count` are `null` when absent):

```json
{"type": "alert.fired", "event_id": "…",
 "rule": {"id", "name", "metric"}, "project": {"id", "name"},
 "value": 0.083, "count": 120, "threshold": 0.05, "comparator": "gt", "window": "5m",
 "window_from": "…", "window_to": "…", "occurred_at": "…",
 "url": "https://…/<org>/<project>/traces?from=…&to=…&filter=…",
 "rule_url": "https://…/<org>/<project>/alerts/<rule id>"}
```

Deliveries go through `notification_deliveries`: a loop every 10 s sends due ones (at most 4 at once, 10 s timeout). Retryable failures (timeouts, connection errors, 408, 429, 5xx, SMTP connection, TLS and authentication errors) wait `min(30 s · 2^(attempts−1), 1 h)`, up to 8 attempts; other failures fail at once. Delivery is at least once. Channels are never disabled automatically: the UI shows each channel's last finished delivery, and "Send test" sends one sample message synchronously (D-K-7). A test send is limited to 5 per minute per user and 5 per minute per channel (`429 rate_limited`, D-K-19). An email over the org's daily cap fails with `daily alert email limit reached`; the cap and the member rule are D-K-19 and D-K-21. Each message reserves one unit of the cap just before it is sent, so a failed send counts only the messages it attempted; the member rule skips disabled users.

**Issue notifications** (D-IC-21). An issue source has its own channels (`issue_source_channels`, at most 10 of the project's channels). They receive the source's new and regressed issues, and no alert. Each source sends at most one digest every 10 minutes to each of its channels, through the deliveries, retries and limits above. Each tick collects the `created` and `regressed` issue events of the source that are after its cut-off (`last_notified_at`) and at least 60 seconds old, so an event whose transaction committed a moment after its time is not lost. An event's time is the clock read inside the transaction that inserts it, never a time read before slow work such as naming a cluster, so it is never older than a cut-off set while that work ran. The cut-off then moves to the newest event collected. The 10 minutes run from that event's time, which is up to 60 seconds before the send, so two digests of one source are at least about 9 to 10 minutes apart. Enabling a source, or giving it its first channel, sets the cut-off to now, so older issues are never sent. A paused source still notifies, and a disabled source does not. Events of an issue that is ignored or merged by then are sent too.

A digest holds up to 20 items, newest first, and `more` counts the rest. Its webhook body has `type` `issues.detected`:

```json
{"type": "issues.detected", "notification_id": "…", "occurred_at": "…",
 "source_id": "…", "source_name": "…", "project": {"id", "name"},
 "items": [{"issue_id": "…", "kind": "created", "title": "…"}],
 "more": 0, "issues_url": "https://…/<org>/<project>/issues?source=<source id>"}
```

Items carry no per-item URL, and `kind` is `created` or `regressed`. Slack shows the header "N new or regressed issues in <source>" (the singular "1 new or regressed issue in <source>" for one), one line per item that links to the issue, "and N more" when `more` is above 0, and a **View issues** button to `issues_url`. Email sends one message per recipient, with the subject `[<product>] N new or regressed issues: <source name>` (singular for one), and the same lines. The email rules above apply, including the org's daily cap. Titles are cut to 100 characters in Slack and email; the webhook carries the whole title. **Send test** on a channel still sends the alert sample.

History: events are kept 90 days and finished deliveries 30 days, pruned hourly (D-K-10). Issue digests are kept 90 days, like events, and their deliveries go with them (D-IC-21).

Online-judge rule pauses do not raise alerts; track I shows them as banners (D-K-12, D-I-14).
