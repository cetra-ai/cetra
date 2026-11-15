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

### Fixed

- Promoted string columns (`session_id`, `user_id`, `environment`, `release`, `request_model`, `response_model`, `provider`, `operation`, `request_params`, service and scope names, `tags`, metadata values, event names and link attributes) are cut to `max_attribute_bytes` like other strings, and listed in `truncated`. Before, a multi-megabyte `session.id` was stored whole.
- The built-in price table prices Gemini (Google AI and Vertex AI), Azure OpenAI, Azure AI Inference, xAI and Mistral spans. It keeps one row per model and provider, and the Google GenAI and OpenInference provider names are recognised (D-B1-11).
