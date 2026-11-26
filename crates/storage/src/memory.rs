//! Query memory budgets and process RAM / cgroup detection (spec 03 §6, §10).
//!
//! Per-query admission and scoped memory pools live in
//! `engine::admission::AsyncAdmissionPool` and `cetra_column::ScopedPool`.

use crate::config::StorageConfig;

/// Base memory of the engine outside every budget (runtimes, catalog, caches' indexes).
pub(crate) const BASE_BYTES: u64 = 200 << 20;

/// Memory the process can use: the machine's RAM, or the limit of the process's own cgroup (and
/// its ancestors) when that is lower (`sysinfo::Process::cgroup_limits`). A systemd unit's or
/// scope's `MemoryMax=` is seen here; a container's limit is seen when its cgroup namespace is
/// the process's root. Zero when the OS reports nothing, including on a platform `sysinfo`
/// does not support.
pub(crate) fn detected_memory() -> u64 {
    use sysinfo::{MemoryRefreshKind, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};
    let Ok(mut system) = System::new_with_specifics(
        RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
    ) else {
        return 0;
    };
    let mut total = system.total_memory();
    if let Ok(pid) = sysinfo::get_current_pid() {
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            false,
            ProcessRefreshKind::nothing(),
        );
        if let Some(limits) = system
            .process(pid)
            .and_then(sysinfo::Process::cgroup_limits)
            && limits.total_memory > 0
        {
            total = total.min(limits.total_memory);
        }
    }
    total
}

/// Total memory for the query pool default (spec 03 §6): half of the detected memory, at most
/// 8 GiB.
///
/// Zero when the OS reports nothing; `Storage::open` then uses `max_memory`.
pub(crate) fn default_total_memory() -> u64 {
    (detected_memory() / 2).min(8 << 30)
}

/// What the storage's budgets can hold at once (spec 03 § Resource limits): two memtables of
/// `memtable.hard_limit` (spans and scores, each on its own), two flush copies of
/// `flush.max_bytes` (the copy each table's flush sorts), `compaction.max_memory`,
/// `query.metadata_cache_bytes`, the query pool `query_total`, and [`BASE_BYTES`]. Saturating.
pub(crate) fn budget_bytes(config: &StorageConfig, query_total: u64) -> u64 {
    let memtables = config.memtable.hard_limit.0.saturating_mul(2);
    let flushes = config.flush.max_bytes.0.saturating_mul(2);
    [
        memtables,
        flushes,
        config.compaction.max_memory.0,
        config.query.metadata_cache_bytes.0,
        query_total,
        BASE_BYTES,
    ]
    .into_iter()
    .fold(0, u64::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;
    const MIB64: u64 = 1 << 20;

    /// A config whose budgets are the ones the budget test sums.
    fn budget_config() -> StorageConfig {
        use crate::config::{CompactionConfig, FlushConfig, MemtableConfig, QueryConfig, S3Config};
        use bytesize::ByteSize;
        StorageConfig {
            delete_objects: true,
            data_dir: std::path::PathBuf::from("unused"),
            min_free_bytes: ByteSize::b(0),
            object_store: None,
            s3: S3Config::default(),
            wal: Default::default(),
            flush: FlushConfig {
                max_bytes: ByteSize::mib(256),
                ..FlushConfig::default()
            },
            memtable: MemtableConfig {
                hard_limit: ByteSize::gib(1),
            },
            query: QueryConfig {
                metadata_cache_bytes: ByteSize::mib(512),
                ..QueryConfig::default()
            },
            anchor: Default::default(),
            gc: Default::default(),
            compaction: CompactionConfig {
                max_memory: ByteSize::gib(1),
                ..CompactionConfig::default()
            },
            deletion: Default::default(),
        }
    }

    #[test]
    fn the_budget_adds_every_component() {
        let config = budget_config();
        assert_eq!(
            budget_bytes(&config, 2 * GIB),
            2 * GIB + 512 * MIB64 + GIB + 512 * MIB64 + 2 * GIB + BASE_BYTES
        );
    }

    #[test]
    fn the_budget_saturates() {
        let mut config = budget_config();
        config.memtable.hard_limit = bytesize::ByteSize(u64::MAX / 2);
        assert_eq!(budget_bytes(&config, 0), u64::MAX);
    }
}
