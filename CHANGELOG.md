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
- `llmobs admin backup` takes a consistent backup of a running server (local mode: hard-linked
  objects; S3 mode: metadata, restorable for `storage.gc.grace`, which may now be up to 30 days).
- `llmobs admin gc-orphans` cleans the object store after a restore and reports missing objects.
- `llmobs admin migrate` migrates `meta.db` and `catalog.db` while the server is stopped, before a
  restore's deletions are re-applied.
- The server keeps a copy of `meta.db` and `catalog.db` before migrating them.
- Helm chart values `extraVolumes` and `extraVolumeMounts` (for example a backup volume).
