//! Bounded LRU cache for decoded Statam file metadata (`StatamFileMeta`) and
//! `.stpk` chunk pack TOCs (`PackToc`), weighted by `memory_bytes()`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use cetra_statam::{PackToc, StatamFileMeta};

use crate::catalog::FileId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum MetaCacheKey {
    File(FileId),
    Pack([u8; 16]),
}

#[derive(Debug, Clone)]
enum MetaCacheValue {
    Statam(Arc<StatamFileMeta>),
    Pack(Arc<PackToc>),
}

impl MetaCacheValue {
    fn memory_bytes(&self) -> usize {
        match self {
            Self::Statam(meta) => meta.memory_bytes(),
            Self::Pack(toc) => toc.memory_bytes(),
        }
    }
}

#[derive(Debug)]
struct CacheEntry {
    value: MetaCacheValue,
    bytes: usize,
    tick: u64,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<MetaCacheKey, CacheEntry>,
    order: BTreeMap<u64, MetaCacheKey>,
    next_tick: u64,
    bytes: usize,
}

impl CacheState {
    fn next_tick(&mut self) -> u64 {
        let tick = self.next_tick;
        self.next_tick = self.next_tick.wrapping_add(1);
        tick
    }

    fn remove_entry(&mut self, key: MetaCacheKey) {
        if let Some(old) = self.entries.remove(&key) {
            self.order.remove(&old.tick);
            self.bytes = self.bytes.saturating_sub(old.bytes);
        }
    }
}

/// Bounded LRU cache of Statam file Hot Tails and `.stpk` Pack TOCs, weighted by
/// `memory_bytes()`. `max_bytes == 0` disables caching.
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

    fn get_raw(&self, key: MetaCacheKey) -> Option<MetaCacheValue> {
        if self.max_bytes == 0 {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let mut state = self.lock();
        if !state.entries.contains_key(&key) {
            drop(state);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let new_tick = state.next_tick();
        let entry = state.entries.get_mut(&key)?;
        let old_tick = entry.tick;
        entry.tick = new_tick;
        let value = entry.value.clone();
        state.order.remove(&old_tick);
        state.order.insert(new_tick, key);
        drop(state);
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(value)
    }

    fn insert_raw(&self, key: MetaCacheKey, value: MetaCacheValue) {
        if self.max_bytes == 0 {
            return;
        }
        let weight = value.memory_bytes().max(1);
        let mut state = self.lock();
        state.remove_entry(key);
        if weight > self.max_bytes {
            return;
        }
        while state.bytes.saturating_add(weight) > self.max_bytes {
            let Some((_, oldest_key)) = state.order.pop_first() else {
                break;
            };
            if let Some(old) = state.entries.remove(&oldest_key) {
                state.bytes = state.bytes.saturating_sub(old.bytes);
            }
        }
        let tick = state.next_tick();
        state.order.insert(tick, key);
        state.entries.insert(
            key,
            CacheEntry {
                value,
                bytes: weight,
                tick,
            },
        );
        state.bytes = state.bytes.saturating_add(weight);
    }

    /// Looks up cached `.statam` metadata for `id`, refreshing its LRU recency on hit.
    pub(crate) fn get_statam(&self, id: FileId) -> Option<Arc<StatamFileMeta>> {
        match self.get_raw(MetaCacheKey::File(id))? {
            MetaCacheValue::Statam(meta) => Some(meta),
            MetaCacheValue::Pack(_) => None,
        }
    }

    /// Inserts or replaces `.statam` metadata for `id`.
    pub(crate) fn insert_statam(&self, id: FileId, meta: Arc<StatamFileMeta>) {
        self.insert_raw(MetaCacheKey::File(id), MetaCacheValue::Statam(meta));
    }

    /// Looks up cached `.stpk` pack TOC for `pack_id`, refreshing its LRU recency on hit.
    pub(crate) fn get_pack(&self, pack_id: [u8; 16]) -> Option<Arc<PackToc>> {
        match self.get_raw(MetaCacheKey::Pack(pack_id))? {
            MetaCacheValue::Pack(toc) => Some(toc),
            MetaCacheValue::Statam(_) => None,
        }
    }

    /// Inserts or replaces `.stpk` pack TOC for `pack_id`.
    pub(crate) fn insert_pack(&self, pack_id: [u8; 16], toc: Arc<PackToc>) {
        self.insert_raw(MetaCacheKey::Pack(pack_id), MetaCacheValue::Pack(toc));
    }

    /// Removes `.statam` metadata for `id` from the cache if present.
    pub(crate) fn invalidate_file(&self, id: FileId) {
        let mut state = self.lock();
        state.remove_entry(MetaCacheKey::File(id));
    }

    /// Removes `.stpk` pack TOC for `pack_id` from the cache if present.
    pub(crate) fn invalidate_pack(&self, pack_id: [u8; 16]) {
        let mut state = self.lock();
        state.remove_entry(MetaCacheKey::Pack(pack_id));
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
    use std::io::Cursor;
    use std::sync::Arc;

    use cetra_column::{Batch, Column, DataType, Field, FlatColumn, FlatData, Schema};
    use cetra_statam::{
        ChunkPackBuilder, PackToc, PackWriteOptions, SliceReader, StatamFileMeta, StatamWriter,
        TableKind, WriteOptions, WriteProfile,
    };
    use uuid::Uuid;

    use super::*;

    fn sample_statam_meta(num_rows: usize) -> Arc<StatamFileMeta> {
        let schema = Schema::try_new_arc(vec![Field::new(
            "start_time",
            DataType::TimestampNs { utc: true },
            false,
        )])
        .unwrap();
        let values: Vec<i64> = (0..num_rows as i64).collect();
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![Column::Flat(
                FlatColumn::try_new(FlatData::TimestampNs { values, utc: true }, None).unwrap(),
            )],
            num_rows,
        )
        .unwrap();
        let mut buf = Cursor::new(Vec::new());
        let opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast);
        let mut w = StatamWriter::new(&mut buf, schema, opts, None).unwrap();
        w.write_batch(&batch).unwrap();
        w.finish().unwrap();
        let rr = SliceReader::new(buf.into_inner());
        Arc::new(StatamFileMeta::read_from(&rr, None).unwrap())
    }

    fn sample_pack_toc(pack_id: [u8; 16]) -> Arc<PackToc> {
        let mut builder = ChunkPackBuilder::new(pack_id, PackWriteOptions::default());
        builder
            .intern_chunk(b"hello statam payload chunk data")
            .unwrap();
        let mut buf = Cursor::new(Vec::new());
        builder.finish(&mut buf).unwrap();
        let rr = SliceReader::new(buf.into_inner());
        PackToc::read_from_with_hint(&rr, None).unwrap().0
    }

    fn id(n: u128) -> FileId {
        FileId(Uuid::from_u128(n))
    }

    #[test]
    fn lru_eviction_and_zero_disables_cache() {
        let m1 = sample_statam_meta(10);
        let m2 = sample_statam_meta(20);
        let m3 = sample_statam_meta(30);
        let single_size = m1.memory_bytes();
        assert!(single_size > 0);
        assert_eq!(single_size, m2.memory_bytes());
        assert_eq!(single_size, m3.memory_bytes());

        // Zero capacity disables caching completely.
        let disabled = MetadataCache::new(0);
        disabled.insert_statam(id(1), Arc::clone(&m1));
        assert_eq!(disabled.bytes(), 0);
        assert!(disabled.get_statam(id(1)).is_none());
        assert_eq!(disabled.hits(), 0);
        assert_eq!(disabled.misses(), 1);

        // Capacity for 2 entries: inserting 3 evicts the least-recently-used.
        let cache = MetadataCache::new(single_size * 2);
        cache.insert_statam(id(1), Arc::clone(&m1));
        cache.insert_statam(id(2), Arc::clone(&m2));
        assert_eq!(cache.bytes(), single_size * 2);

        // Touch id(1) so id(2) becomes the LRU entry.
        assert_eq!(cache.get_statam(id(1)).unwrap().postscript.row_count, 10);
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 0);

        // Inserting id(3) evicts id(2), keeping id(1) and id(3).
        cache.insert_statam(id(3), Arc::clone(&m3));
        assert_eq!(cache.bytes(), single_size * 2);
        assert!(cache.get_statam(id(2)).is_none());
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.get_statam(id(1)).unwrap().postscript.row_count, 10);
        assert_eq!(cache.get_statam(id(3)).unwrap().postscript.row_count, 30);
        assert_eq!(cache.hits(), 3);

        cache.invalidate_file(id(1));
        assert_eq!(cache.bytes(), single_size);
        assert!(cache.get_statam(id(1)).is_none());
        assert_eq!(cache.misses(), 2);
    }

    #[test]
    fn caches_and_invalidates_pack_tocs_alongside_statam_meta() {
        let m1 = sample_statam_meta(5);
        let p1 = sample_pack_toc([1u8; 16]);
        let total = m1.memory_bytes() + p1.memory_bytes();
        let cache = MetadataCache::new(total);

        cache.insert_statam(id(1), Arc::clone(&m1));
        cache.insert_pack([1u8; 16], Arc::clone(&p1));
        assert_eq!(cache.bytes(), total);
        assert_eq!(cache.get_statam(id(1)).unwrap().postscript.row_count, 5);
        assert_eq!(
            cache.get_pack([1u8; 16]).unwrap().postscript.pack_id,
            [1u8; 16]
        );

        cache.invalidate_pack([1u8; 16]);
        assert!(cache.get_pack([1u8; 16]).is_none());
        assert_eq!(cache.bytes(), m1.memory_bytes());
        assert!(cache.get_statam(id(1)).is_some());
    }
}
