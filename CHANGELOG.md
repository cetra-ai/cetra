# Changelog

All notable changes to llmobs are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (D-N-5). A release that needs action on upgrade starts
with an `### Upgrade notes` section (docs/self-hosting/upgrade.md).

## [Unreleased]

### Added

- Storage: a failed fsync of a local object stops local object writes until a restart, and `StorageHealth::objects_sync_failed` reports it (D-R-6). New error `ObjectError::SyncFailed`.
- Storage: a `catalog.db` commit that fails with an I/O error stops the catalog until a restart, and `StorageHealth::catalog_failed` reports it (D-R-6). New errors `CatalogError::Stopped` and `CommitError::Stopped`.
- Release archives for Linux x86_64 and aarch64 (static) and macOS arm64, an install script, a
  Docker image on GHCR, docker-compose files (local volume, or S3 with Garage) and a Helm chart,
  with checksums, build provenance attestations, SBOMs and third-party licence notices (track N).
- The S3 self-hosting guide recommends a lifecycle rule that aborts incomplete multipart uploads (track R).
- Storage: a warning at start when the storage's memory budgets add up to more than the memory limit.

### Fixed

- The default query memory pool now honours a systemd `MemoryMax=` (or any cgroup limit of the process), not only a container's limit.
- A local object write that fails, for example on a full disk, no longer leaves its temporary file until the next start.
- A query that would spill past the free disk above `storage.min_free_bytes` now fails as too large (422) instead of filling the disk.
- With S3, a multipart upload that fails is now aborted, so its parts no longer stay on the bucket (track R).
- With S3, a file that passes verification has its read-cache blocks dropped, so a bad cached block no longer keeps failing queries (track R).
- Storage: a local object deleted by garbage collection, or a directory created just before a crash, could reappear or disappear after a power loss.
- Storage: in interval mode (`wal.sync = "<duration>"`), a power loss could leave a torn WAL segment that was no longer the last one, and the next start refused it as corrupt. A segment is now fsynced before a newer one is created.
- Storage: a WAL group whose fsync failed is no longer truncated with a second fsync, and the segment a failed group discards stays removed after a power loss.
- Storage: a read-cache size file torn by a power loss could make S3-mode queries read short ranges. The size file now carries a CRC-32C; cache directories written in the old format are dropped at open and fetched again.
- The span list no longer shows a re-sent span twice, and its pages no longer depend on the page size. A re-send that changed its `start_time` is still listed twice until compaction merges them.
