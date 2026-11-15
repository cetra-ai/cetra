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

- Storage: a local object deleted by garbage collection, or a directory created just before a crash, could reappear or disappear after a power loss.
- Storage: in interval mode (`wal.sync = "<duration>"`), a power loss could leave a torn WAL segment that was no longer the last one, and the next start refused it as corrupt. A segment is now fsynced before a newer one is created.
- The span list no longer shows a re-sent span twice, and its pages no longer depend on the page size. A re-send that changed its `start_time` is still listed twice until compaction merges them.
