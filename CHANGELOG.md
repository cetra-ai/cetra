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

- SQL console answers and exports are bounded in bytes: a JSON answer over 32 MiB of encoded
  output, or a query whose encoding does not fit the query memory pool, is `422 query_too_large`
  (D98). Aliasing a heavy column or cross-joining heavy rows could previously multiply the output
  past any memory limit.
