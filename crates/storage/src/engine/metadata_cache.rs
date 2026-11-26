//! Bounded LRU cache for decoded `ParquetMetaData` keyed by `FileId`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use parquet::file::metadata::ParquetMetaData;

use crate::catalog::FileId;

#[derive(Debug)]
struct CacheEntry {
    meta: Arc<ParquetMetaData>,
    bytes: usize,
    tick: u64,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<FileId, CacheEntry>,
    order: BTreeMap<u64, FileId>,
    next_tick: u64,
    bytes: usize,
}

impl CacheState {
    fn next_tick(&mut self) -> u64 {
        let tick = self.next_tick;
        self.next_tick = self.next_tick.wrapping_add(1);
        tick
    }

    fn remove_entry(&mut self, id: FileId) {
        if let Some(old) = self.entries.remove(&id) {
            self.order.remove(&old.tick);
            self.bytes = self.bytes.saturating_sub(old.bytes);
        }
    }
}

/// Bounded LRU cache of Parquet file footers + page indexes, weighted by
/// `ParquetMetaData::memory_size()`. `max_bytes == 0` disables caching.
#[derive(Debug)]
pub(crate) struct MetadataCache {
    max_bytes: usize,
    state: Mutex<CacheState>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl MetadataCache {
    /// Creates a metadata cache bounded at `max_bytes` (`0` disables caching).
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            state: Mutex::new(CacheState::default()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, CacheState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Looks up cached metadata for `id`, refreshing its LRU recency on hit.
    pub(crate) fn get(&self, id: FileId) -> Option<Arc<ParquetMetaData>> {
        if self.max_bytes == 0 {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let mut state = self.lock();
        if !state.entries.contains_key(&id) {
            drop(state);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let new_tick = state.next_tick();
        let entry = state.entries.get_mut(&id)?;
        let old_tick = entry.tick;
        entry.tick = new_tick;
        let meta = Arc::clone(&entry.meta);
        state.order.remove(&old_tick);
        state.order.insert(new_tick, id);
        drop(state);
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(meta)
    }

    /// Inserts or replaces metadata for `id`, evicting LRU entries while `bytes > max_bytes`.
    pub(crate) fn insert(&self, id: FileId, meta: Arc<ParquetMetaData>) {
        if self.max_bytes == 0 {
            return;
        }
        let weight = meta.memory_size();
        let mut state = self.lock();
        state.remove_entry(id);
        if weight > self.max_bytes {
            return;
        }
        while state.bytes.saturating_add(weight) > self.max_bytes {
            let Some((_, oldest_id)) = state.order.pop_first() else {
                break;
            };
            if let Some(old) = state.entries.remove(&oldest_id) {
                state.bytes = state.bytes.saturating_sub(old.bytes);
            }
        }
        let tick = state.next_tick();
        state.order.insert(tick, id);
        state.entries.insert(
            id,
            CacheEntry {
                meta,
                bytes: weight,
                tick,
            },
        );
        state.bytes = state.bytes.saturating_add(weight);
    }

    /// Removes `id` from the cache if present.
    #[cfg(test)]
    pub(crate) fn invalidate(&self, id: FileId) {
        let mut state = self.lock();
        state.remove_entry(id);
    }

    /// Currently cached metadata bytes.
    pub(crate) fn bytes(&self) -> usize {
        self.lock().bytes
    }

    /// Total cache hits.
    #[cfg(test)]
    pub(crate) fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Total cache misses.
    #[cfg(test)]
    pub(crate) fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use parquet::file::metadata::{FileMetaData, ParquetMetaData};
    use parquet::schema::types::Type;
    use uuid::Uuid;

    use super::*;

    fn sample_metadata(num_rows: i64) -> Arc<ParquetMetaData> {
        let schema = Arc::new(
            Type::group_type_builder("schema")
                .build()
                .expect("empty group schema"),
        );
        let descr = Arc::new(parquet::schema::types::SchemaDescriptor::new(schema));
        let file_meta = FileMetaData::new(1, num_rows, None, None, descr, None);
        let meta = ParquetMetaData::new(file_meta, Vec::new());
        Arc::new(meta)
    }

    fn id(n: u128) -> FileId {
        FileId(Uuid::from_u128(n))
    }

    #[test]
    fn lru_eviction_and_zero_disables_cache() {
        let m1 = sample_metadata(10);
        let m2 = sample_metadata(20);
        let m3 = sample_metadata(30);
        let single_size = m1.memory_size();
        assert!(single_size > 0);

        // Zero capacity disables caching completely.
        let disabled = MetadataCache::new(0);
        disabled.insert(id(1), Arc::clone(&m1));
        assert_eq!(disabled.bytes(), 0);
        assert!(disabled.get(id(1)).is_none());
        assert_eq!(disabled.hits(), 0);
        assert_eq!(disabled.misses(), 1);

        // Capacity for 2 entries: inserting 3 evicts the least-recently-used.
        let cache = MetadataCache::new(single_size * 2);
        cache.insert(id(1), Arc::clone(&m1));
        cache.insert(id(2), Arc::clone(&m2));
        assert_eq!(cache.bytes(), single_size * 2);

        // Touch id(1) so id(2) becomes the LRU entry.
        assert_eq!(cache.get(id(1)).unwrap().file_metadata().num_rows(), 10);
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 0);

        // Inserting id(3) evicts id(2), keeping id(1) and id(3).
        cache.insert(id(3), Arc::clone(&m3));
        assert_eq!(cache.bytes(), single_size * 2);
        assert!(cache.get(id(2)).is_none());
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.get(id(1)).unwrap().file_metadata().num_rows(), 10);
        assert_eq!(cache.get(id(3)).unwrap().file_metadata().num_rows(), 30);
        assert_eq!(cache.hits(), 3);

        cache.invalidate(id(1));
        assert_eq!(cache.bytes(), single_size);
        assert!(cache.get(id(1)).is_none());
        assert_eq!(cache.misses(), 2);
    }
}
