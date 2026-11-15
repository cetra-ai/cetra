# Changelog

All notable changes to llmobs are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (D-N-5). A release that needs action on upgrade starts
with an `### Upgrade notes` section (docs/self-hosting/upgrade.md).

## [Unreleased]

### Upgrade notes

- With `auth.signup = open`, an email alert channel that lists an address which is not a verified
  member of its org is refused on its next edit, and its deliveries skip that address and are
  recorded as failed. Change the recipients, or use `invite_only` signup.
- A trace `latency_p95` alert rule whose threshold is at or above its window is refused on its next
  edit; lower the threshold or widen the window.

### Added

- Release archives for Linux x86_64 and aarch64 (static) and macOS arm64, an install script, a
  Docker image on GHCR, docker-compose files (local volume, or S3 with Garage) and a Helm chart,
  with checksums, build provenance attestations, SBOMs and third-party licence notices (track N).
- `email_verified` and `has_password` on the `User` returned by `GET /api/auth/me`, signup and
  setup.

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

- OTLP/gRPC checks the credential, the project and the units cap before decoding a message, and
  reserves the in-flight budget for the largest message before the body is read. The gRPC listener
  allows 100 concurrent streams and 32 in-flight requests per connection.
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
