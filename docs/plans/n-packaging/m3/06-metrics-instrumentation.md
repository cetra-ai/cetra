# 06: count HTTP, ingest, eval, alert and email events

Read [00-design.md](00-design.md) first, especially § `/metrics` and its label rule (D-N-29).

**Goal:** the event metrics that phase 05 registered start counting:
- HTTP requests by route template;
- OTLP ingest requests, spans and bytes;
- finished eval jobs and their run time;
- alert evaluations and deliveries;
- emails.

**Edit only:**
- `crates/server/src/telemetry.rs` (small `record_*` helpers only);
- `crates/server/src/http/mod.rs` (the layer, in `finish()`);
- `crates/server/src/routes/otlp.rs` (the OTLP HTTP handler: `export`, `export_body`), `crates/server/src/ingest/mod.rs` and `ingest/grpc.rs`;
- `crates/server/src/eval/dispatch.rs` or `eval/execute.rs`, where a job reaches a final status;
- `crates/server/src/alerts/evaluate.rs` and `alerts/deliver.rs`;
- `crates/server/src/email/mod.rs`, and the callers of `Mailer::from_config` and `Mailer::stub` (`serve.rs`, `test_support/mod.rs`, the `config.rs` and `email` tests such as `mailer_follows_smtp_url`).

**Commit:** `Count HTTP, ingest, eval, alert and email events`

**Needs:** 05.

## Read first

- `crates/server/src/telemetry.rs` (phase 05): the fields and label enums.
- `crates/server/src/http/mod.rs`: `finish()` (near line 170), which builds both the main and the OTLP HTTP router, and `log_request`, which already reads `MatchedPath`.
- `crates/server/src/routes/otlp.rs`: `export` and `export_body`, how they map errors to statuses (including `QuotaExceeded`), and where the accepted span count and body size are known.
- `crates/server/src/ingest/mod.rs`: `IngestError`.
- `crates/server/src/ingest/grpc.rs`: the export handler and its `tonic::Status` mapping.
- `crates/server/src/meta/eval_jobs.rs` lines 595–640: the `done` and `failed` updates and the `skipped` path. Then find their callers in `eval/`.
- `crates/server/src/alerts/evaluate.rs` `evaluate_once` and its report; `alerts/deliver.rs` `send_one`.
- `crates/server/src/email/mod.rs` `send_now`, `send_in_background`.

## Changes

Keep every increment one line at the place where the event is already known. Do not restructure the code to reach a value; if a value is not available where the event happens, say so in the report and leave that label out.

### HTTP (`cetra_http_requests_total`, `cetra_http_request_duration_seconds`)

- A middleware function `telemetry::track_http(State(state), MatchedPath (optional), req, next)`, built with `axum::middleware::from_fn_with_state`, is added once in `finish()`, next to the layer that runs `log_request`, so both the main and the OTLP HTTP router get it. It must be inside the routing (where `MatchedPath` is set), as `log_request` is.
- `route` is the `MatchedPath` string, or `"other"` when the request matched no route. The UI fallback and its assets also give `"other"`.
- `method` is one of `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, or `OTHER`.
- `status` is `"1xx"`…`"5xx"` from the response.
- The duration is measured from before `next.run` to the response. A streamed body (SSE) is counted when its headers are sent. That is acceptable, and the doc comment says so.

### Ingest (`cetra_ingest_requests_total`, `_spans_total`, `_bytes_total`)

**`protocol`:** `otlp_http` for the OTLP HTTP handler (on either router), `otlp_grpc` for the gRPC export.

**`outcome`:** from the `IngestError` variant where the handler has one, otherwise from the final status.

| Outcome | From |
|---|---|
| `ok` | success |
| `invalid` | `Body` (bad content), `Decode`, `TooManySpans`, `Batch`, `MediaHash`; HTTP 400/415/422, gRPC `InvalidArgument` |
| `unauthorized` | auth failures; HTTP 401/403, gRPC `Unauthenticated`/`PermissionDenied` |
| `too_large` | the body-size refusal; HTTP 413 |
| `rate_limited` | `RateLimited`, `Busy`; HTTP 429 |
| `quota` | the monthly cap; HTTP 402 |
| `disk_full` | the append refused for low disk (find the `AppendError` variant) |
| `unavailable` | everything else |

Auth and size checks may run in an extractor or middleware before the handler. If the handler never sees those requests, count them where they are refused, or leave them to the HTTP route counter and say so. Do not add a second auth path.

**`_spans_total` and `_bytes_total`** count accepted requests only: the span count the handler appends, and the request body length in bytes (compressed, as received).

### Eval (`cetra_eval_jobs_finished_total`, `cetra_eval_run_duration_seconds`)

- Where a job's final status is written (`done`, `failed`, `skipped`), increment `outcome`. **Count after the write succeeds.** A lost claim (the update matched no row) is not counted.
- Where an `EvalRunner::run` (or the check executor) returns, observe the run's wall time under `kind`:
  - `llm_judge`;
  - `webhook`;
  - `check` (code checks, if they go through the same path).

  Use the evaluator kind the dispatcher already has.

### Alerts

- `cetra_alert_evaluations_total{outcome}`: in `evaluate_once`, add `report.evaluated - report.failed` to `ok` and `report.failed` to `failed`.
- `cetra_alert_deliveries_total{channel,outcome}`: in `send_one`, after each attempt:
  - `channel` from the channel's `ChannelKind` (`email`, `webhook`, `slack`);
  - `outcome` `sent` or `failed`.

### Email (`cetra_emails_total{outcome}`)

The `Mailer` has no `AppState`.
- Give `Mailer` a field `emails: Family<EmailOutcomeLabel, Counter>`. A `Family` is `Arc`-backed, so a clone counts into the same series.
- `Mailer::from_config(config, emails)` and `Mailer::stub(emails)` take it.
- In `bind` and `TestApp`, build `Telemetry` first and pass `telemetry.emails.clone()`.
- Tests that do not care pass `Family::default()`.
- Update every caller (`grep -rn "Mailer::from_config\|Mailer::stub" crates/server`).

| Outcome | When |
|---|---|
| `sent` | the transport accepted the message |
| `failed` | a transport error, after retries for background sends |
| `refused` | `Transport::Off` (restore mode) |

## Tests

**`telemetry.rs` or `http/mod.rs`:**
- `http_requests_are_counted_by_route`:
  - `GET /api/auth/config` twice on a `TestApp`;
  - `render` contains `cetra_http_requests_total{route="/api/auth/config",method="GET",status="2xx"} 2`;
  - a `GET /nope/xyz` is counted under `route="other"`.

**`ingest/mod.rs`:**
- `accepted_otlp_request_is_counted`: post a fixture (the existing ingest tests show how). Then `cetra_ingest_requests_total{protocol="otlp_http",outcome="ok"} 1`, `cetra_ingest_spans_total{protocol="otlp_http"}` equals the fixture's span count, and the bytes total is > 0.
- `invalid_otlp_request_is_counted`: garbage body → `outcome="invalid"` 1, spans 0.

**`ingest/grpc.rs`:** `accepted_grpc_export_is_counted`, same as the HTTP case with `protocol="otlp_grpc"`.

**Eval:** in an existing dispatcher test that runs a job to `done` and one to `failed`, assert the two counters, and that `cetra_eval_run_duration_seconds_count{kind=…}` ≥ 1.

**Alerts:**
- In an existing delivery test with a webhook channel that answers 200, assert `cetra_alert_deliveries_total{channel="webhook",outcome="sent"} 1`.
- With a failing one, assert `outcome="failed"`.

**Email:**
- With the test mailer, one `send_now` gives `outcome="sent"` 1.
- With phase 03's restore-mode mailer, `outcome="refused"` 1.

**Label rule:** extend phase 05's `labels_carry_no_tenant_data` to run **after** these tests' kind of traffic: an ingest, an HTTP call with an org slug in the path, and an alert delivery. The output contains no slug, no UUID of an org or project, and no email.
- A route template like `/api/v1/orgs/{org}/…` is fine.
- A rendered UUID would mean a raw path leaked.

Assert with a regex that no `[0-9a-f]{8}-[0-9a-f]{4}-` appears.

## Verify

```bash
cargo test -p cetra-server --all-features counted telemetry
just check
```

## Done when

- [ ] Every event metric of the 00-design table (except phase 07's) moves under the matching traffic.
- [ ] The tests pass, including the no-tenant-data check.
- [ ] `just check` passes. One commit.
