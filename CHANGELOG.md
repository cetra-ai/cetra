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

### Changed

- A dropped organization's daily usage is kept 40 days after the drop, for billing, then deleted.

### Upgrade notes

- Cloud cells must set `entitlements.control_plane = true`; without it, `/api/internal/v1/*`
  answers 404 and org and control-plane tokens are refused (D-P-50).
