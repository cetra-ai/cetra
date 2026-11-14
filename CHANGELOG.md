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

- The traces page shows onboarding only for a project with no trace at all (an all-time one-row probe); a project whose traces are older than the selected range sees its empty range instead.
- The live traces list refetches its first page at most once at a time: a busy stream no longer cancels the refetch it started, and the traces that arrived meanwhile are fetched by one trailing refetch.
- The live traces list and the trace view stop their stream on a 4xx answer instead of retrying forever, a 401 on a stream goes to `/login`, and the live list opens no stream while it shows an invalid filter.
- The `/cli-auth` page hands the new CLI token over in a form POST instead of a URL, so it stays out of the browser history; the token expires after 90 days, and the page warns about links sent by someone else and shows the CLI's host as reported.
