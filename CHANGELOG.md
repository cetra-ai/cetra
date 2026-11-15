# Changelog

All notable changes to llmobs are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (D-N-5). A release that needs action on upgrade starts
with an `### Upgrade notes` section (docs/self-hosting/upgrade.md).

## [Unreleased]

### Added

- Release archives for Linux x86_64 and aarch64 (static) and macOS arm64, an install script, a
  Docker image on GHCR, docker-compose files (local volume, or S3 with Garage) and a Helm chart,
  with checksums, build provenance attestations, SBOMs and third-party licence notices (track N).
- `GET /api/v1/projects/{project}/saved-views/{id}` returns one saved view by id.

### Fixed

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
