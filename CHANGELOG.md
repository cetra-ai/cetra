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
- Plans with a data-access window hide older traces, spans, scores and media from every read
  (UI, API, SQL, MCP, alerts, evaluators); the time-range picker shows the limit.
- Control-plane org deletion (`DELETE /api/internal/v1/orgs/{org_id}`) and an applied-drops feed
  (`GET /api/internal/v1/deletions`) for cloud cells.
- `llmobs admin backup` takes a consistent backup of a running server (local mode: hard-linked
  objects; S3 mode: metadata, restorable for `storage.gc.grace`, which may now be up to 30 days).
- `llmobs admin gc-orphans` cleans the object store after a restore and reports missing objects.
- `llmobs admin migrate` migrates `meta.db` and `catalog.db` while the server is stopped, before a
  restore's deletions are re-applied.
- The server keeps a copy of `meta.db` and `catalog.db` before migrating them.
- Helm chart values `extraVolumes` and `extraVolumeMounts` (for example a backup volume).

### Fixed

- A media blob whose declared MIME type is longer than 255 bytes is sniffed instead of failing the whole OTLP request. Before, the request got a 400 and the exporter dropped every span in it.
- Promoted string columns (`session_id`, `user_id`, `environment`, `release`, `request_model`, `response_model`, `provider`, `operation`, `request_params`, service and scope names, `tags`, metadata values, event names and link attributes) are cut to `max_attribute_bytes` like other strings, and listed in `truncated`. Before, a multi-megabyte `session.id` was stored whole.
- The built-in price table prices Gemini (Google AI and Vertex AI), Azure OpenAI, Azure AI Inference, xAI and Mistral spans. It keeps one row per model and provider, and the Google GenAI and OpenInference provider names are recognised (D-B1-11).
- The Python SDK parses a score `Retry-After` header as the spec and the TypeScript SDK do: ASCII digits, surrounding whitespace ignored. `+5`, `1_0` and fullwidth digits now use the default backoff, and a value too large to convert is capped at 30 s instead of failing.

### Changed

- A dropped organization's daily usage is kept 40 days after the drop, for billing, then deleted.

### Upgrade notes

- Cloud cells must set `entitlements.control_plane = true`; without it, `/api/internal/v1/*`
  answers 404 and org and control-plane tokens are refused (D-P-50).
