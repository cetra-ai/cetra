# Changelog

All notable changes to llmobs are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (D-N-5). A release that needs action on upgrade starts
with an `### Upgrade notes` section (docs/self-hosting/upgrade.md).

## [Unreleased]

### Upgrade notes

- With `auth.signup = open`, an email alert channel that lists an address which is not a verified,
  enabled member of its org is refused on its next edit, and its deliveries skip that address and are
  recorded as failed. Change the recipients, or use `invite_only` signup.
- A trace `latency_p95` alert rule whose threshold is at or above its window is refused on its next
  edit; lower the threshold or widen the window.

### Added

- Release archives for Linux x86_64 and aarch64 (static) and macOS arm64, an install script, a
  Docker image on GHCR, docker-compose files (local volume, or S3 with Garage) and a Helm chart,
  with checksums, build provenance attestations, SBOMs and third-party licence notices (track N).
- `email_verified` and `has_password` on the `User` returned by `GET /api/auth/me`, signup and
  setup.
- `GET /api/v1/projects/{project}/saved-views/{id}` returns one saved view by id.
- `llmobs admin backup` takes a consistent backup of a running server (local mode: hard-linked
  objects; S3 mode: metadata, restorable for `storage.gc.grace`, which may now be up to 30 days).
- `llmobs admin gc-orphans` cleans the object store after a restore and reports missing objects.
- `llmobs admin migrate` migrates `meta.db` and `catalog.db` while the server is stopped, before a
  restore's deletions are re-applied.
- The server keeps a copy of `meta.db` and `catalog.db` before migrating them.
- Helm chart values `extraVolumes` and `extraVolumeMounts` (for example a backup volume).

### Changed

- Alert email is capped at 1,000 messages per org per UTC day, "Send test" is limited to 5 a minute
  per user and per channel, and when signup is open an email channel may only list verified
  members of its org (D-K-19).
- LLM latency alerts window on the span's end time, so a long call that ends in the window is
  counted; a trace latency threshold at or above the window is refused with 400 (D-K-18).
- `settings` is a reserved project slug (400 on `slug`). `retention_days` on project create and
  update is nullable in the OpenAPI contract, from 1 to 36500; `null` keeps data forever.
- A read that makes several storage calls (a list with scores, a session or user detail) has 30 s
  in total, not 30 s per call.
- `llmobs login` receives the token from the browser as a form POST, not in the callback URL, so
  the token stays out of browser history. A token created by the sign-in page expires after 90
  days; run `llmobs login` again when it does.

### Fixed

- The traces page shows onboarding only for a project with no trace at all (an all-time one-row probe); a project whose traces are older than the selected range sees its empty range instead.
- The live traces list refetches its first page at most once at a time: a busy stream no longer cancels the refetch it started, and the traces that arrived meanwhile are fetched by one trailing refetch.
- The live traces list and the trace view stop their stream on a 4xx answer instead of retrying forever, a 401 on a stream goes to `/login`, and the live list opens no stream while it shows an invalid filter.
- The org's project list says "Forever" for a project that keeps data forever, instead of "Default".
- The `/cli-auth` page hands the new CLI token over in a form POST instead of a URL, so it stays out of the browser history; the token expires after 90 days, and the page warns about links sent by someone else and shows the CLI's host as reported.
- OTLP/gRPC checks the credential, the project and the units cap before decoding a message, and
  reserves the in-flight budget for the largest message before the body is read. The gRPC listener
  allows 100 concurrent streams and 32 in-flight requests per connection, and cuts a request
  after 30 s, so a stalled stream releases its reservation.
- Score alerts count scores created after their trace ended (judge, feedback and annotation scores),
  for traces that started up to 7 days before the window (D-K-17).
- Usage counts are no longer lost when shutdown cuts a usage flush short.
- Identical dashboard metric requests that miss the cache together run the query once, and a full
  cache drops its oldest answers instead of all of them.
- `llmobs login` exits at once when the sign-in is cancelled in the browser, instead of waiting five
  minutes for a callback that never comes.
- A password reset (forgot-password link or admin reset link) now revokes the user's personal
  access tokens as well as their web sessions, as a password change already did.
- A categorical webhook evaluator must list its categories; one saved without them is refused with
  400 (`output_score.categories`) instead of failing every run with `unknown label`.
- A webhook evaluator answering 402 now pauses its rule, as D160 says for every other evaluator
  (it was permanent, so each run failed until the error breaker tripped).
- SQL console answers and exports are bounded in bytes. A JSON answer stops at the last row that
  fits 32 MiB of encoded output and is marked `truncated` like a row-limit cut; only a first row
  over the cap, or an encoding that does not fit the query memory pool, is `422 query_too_large`
  (D98). Aliasing a heavy column or cross-joining heavy rows could previously multiply the output
  past any memory limit.
- CSV exports prefix a string cell or header name that starts with `=`, `+`, `-`, `@`, tab or CR
  with `'`, so a spreadsheet shows it as text instead of running it (D-A2-5). NDJSON and Parquet
  are unchanged.
- The SQL console lists every saved query you can see, not only the first 1,000, and opens a
  `?view=` link to a query past the listed pages.
- Deleted spans and the spans of a dropped project leave the WAL by the end of the next flush cycle,
  not when the newest WAL segment fills (64 MiB): each committed flush starts a new segment when
  the open one holds records, and deletes the segments it has flushed (D-A1-14).
- `llmobs query` finds `{name:Type}` placeholders after a nested `/* … /* … */ … */` comment, as the
  server does, instead of reading the rest of the statement as a string.
- An export whose client stops reading for 30 s releases its query, and its memory admission, instead
  of holding them until the connection closes.
- A media blob whose declared MIME type is longer than 255 bytes is sniffed instead of failing the whole OTLP request. Before, the request got a 400 and the exporter dropped every span in it.
- Promoted string columns (`session_id`, `user_id`, `environment`, `release`, `request_model`, `response_model`, `provider`, `operation`, `request_params`, service and scope names, `tags`, metadata values, event names and link attributes) are cut to `max_attribute_bytes` like other strings, and listed in `truncated`. Before, a multi-megabyte `session.id` was stored whole.
- The built-in price table prices Gemini (Google AI and Vertex AI), Azure OpenAI, Azure AI Inference, xAI and Mistral spans. It keeps one row per model and provider, and the Google GenAI and OpenInference provider names are recognised (D-B1-11).
- The Python SDK parses a score `Retry-After` header as the spec and the TypeScript SDK do: ASCII digits, surrounding whitespace ignored. `+5`, `1_0` and fullwidth digits now use the default backoff, and a value too large to convert is capped at 30 s instead of failing.
