//! Corrupt-file handling: which read errors make a file a suspect, the queue
//! of suspects, and the whole-file check that decides whether one is corrupt
//! (spec 03 §10, D-A1-10).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use arrow::error::ArrowError;
use datafusion::error::DataFusionError;
use object_store::ObjectStore;
use parquet::errors::ParquetError;

use crate::catalog::{self, FileId, FileMeta};
use crate::query_error::root;

/// Whether `error` may come from a corrupt Parquet file rather than the object
/// store or the query (spec 03 §10). False positives only cost a verification.
pub(crate) fn may_be_corrupt(error: &DataFusionError) -> bool {
    match root(error) {
        DataFusionError::ParquetError(error) => !is_store_error(error),
        DataFusionError::ArrowError(error, _) => matches!(**error, ArrowError::ParquetError(_)),
        DataFusionError::External(error) => {
            error
                .downcast_ref::<ParquetError>()
                .is_some_and(|error| !is_store_error(error))
                || error
                    .downcast_ref::<ArrowError>()
                    .is_some_and(|error| matches!(error, ArrowError::ParquetError(_)))
        }
        _ => false,
    }
}

/// A `ParquetError::External` holding an `object_store::Error` other than
/// `NotFound`: the store failed, the file may be fine. A missing object is
/// not a store failure: it makes the file a suspect, and verification
/// finds it corrupt.
pub(crate) fn is_store_error(error: &ParquetError) -> bool {
    match error {
        ParquetError::External(source) => source
            .downcast_ref::<object_store::Error>()
            .is_some_and(|error| !matches!(error, object_store::Error::NotFound { .. })),
        _ => false,
    }
}

/// Files waiting for verification, and files verified healthy recently.
#[derive(Debug, Default)]
pub(crate) struct VerifyQueue {
    state: Mutex<QueueState>,
}

#[derive(Debug, Default)]
struct QueueState {
    pending: VecDeque<FileId>,
    queued: HashSet<FileId>,
    /// File id → when it was last verified healthy, ns.
    healthy: HashMap<FileId, i64>,
}

/// At most this many files wait; more suspects are dropped (and logged once per call).
pub(crate) const QUEUE_MAX: usize = 4096;
/// A file verified healthy is not queued again for this long.
pub(crate) const HEALTHY_FOR: i64 = 60 * 60 * 1_000_000_000;

impl VerifyQueue {
    fn lock(&self) -> MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues every id that is not queued and not verified healthy within
    /// `HEALTHY_FOR` of `now`. Returns how many were queued.
    pub(crate) fn suspect(&self, ids: impl IntoIterator<Item = FileId>, now: i64) -> usize {
        let mut state = self.lock();
        let mut queued = 0_usize;
        let mut dropped = 0_usize;
        for id in ids {
            if state.queued.contains(&id) {
                continue;
            }
            if state
                .healthy
                .get(&id)
                .is_some_and(|at| now.saturating_sub(*at) < HEALTHY_FOR)
            {
                continue;
            }
            if state.pending.len() >= QUEUE_MAX {
                dropped += 1;
                continue;
            }
            state.pending.push_back(id);
            state.queued.insert(id);
            queued += 1;
        }
        drop(state);
        if dropped > 0 {
            tracing::warn!(dropped, "verify queue is full; suspect files dropped");
        }
        queued
    }

    /// Removes and returns up to `max` ids, oldest first.
    pub(crate) fn take(&self, max: usize) -> Vec<FileId> {
        let mut state = self.lock();
        let mut taken = Vec::new();
        while taken.len() < max {
            let Some(id) = state.pending.pop_front() else {
                break;
            };
            state.queued.remove(&id);
            taken.push(id);
        }
        taken
    }

    /// Puts `id` back at the end (unavailable, or claimed by a running job).
    pub(crate) fn requeue(&self, id: FileId) {
        let mut state = self.lock();
        if state.queued.insert(id) {
            state.pending.push_back(id);
        }
    }

    /// Remembers `id` as healthy at `now`, and forgets entries older than `HEALTHY_FOR`.
    pub(crate) fn healthy(&self, id: FileId, now: i64) {
        let mut state = self.lock();
        state.healthy.insert(id, now);
        state
            .healthy
            .retain(|_, at| now.saturating_sub(*at) < HEALTHY_FOR);
    }
}

/// The files one query's scans read, and the queue to report them to (spec 03 §10).
#[derive(Debug, Clone)]
pub struct Suspects {
    inner: Arc<SuspectsInner>,
}

#[derive(Debug)]
struct SuspectsInner {
    /// Files the query's scans read so far.
    files: Mutex<HashSet<FileId>>,
    queue: Arc<VerifyQueue>,
}

impl Suspects {
    pub(crate) fn new(queue: Arc<VerifyQueue>) -> Self {
        Self {
            inner: Arc::new(SuspectsInner {
                files: Mutex::new(HashSet::new()),
                queue,
            }),
        }
    }

    /// Not connected to any storage: reports go nowhere. For `ProjectQuery`
    /// values built by tests.
    #[cfg(test)]
    pub(crate) fn detached() -> Self {
        Self::new(Arc::new(VerifyQueue::default()))
    }

    /// Records the files a scan reads.
    pub(crate) fn record(&self, files: &[Arc<FileMeta>]) {
        let mut recorded = self
            .inner
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        recorded.extend(files.iter().map(|file| file.id));
    }

    /// Queues the recorded files when `error` may come from a corrupt file.
    pub(crate) fn report(&self, error: &DataFusionError) {
        if !may_be_corrupt(error) {
            return;
        }
        let ids: Vec<FileId> = self
            .inner
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .copied()
            .collect();
        let queued = self.inner.queue.suspect(ids, catalog::now_ns());
        if queued > 0 {
            tracing::warn!(
                files = queued,
                "a query failed to decode; its files are queued for verification"
            );
        }
    }

    /// Queues a single corrupt or missing file for background verification.
    pub(crate) fn report_corrupt(&self, id: FileId) {
        let queued = self.inner.queue.suspect([id], catalog::now_ns());
        if queued > 0 {
            tracing::warn!(
                files = queued,
                "a query failed to decode; its files are queued for verification"
            );
        }
    }
}

/// What a whole-file read found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Every row group and column decodes, with the catalog's row count.
    Healthy,
    /// The file is corrupt. `reason` is static.
    Corrupt {
        /// Why, for the log.
        reason: &'static str,
    },
    /// The object store failed. The file may be fine.
    Unavailable,
}

/// Reads `file` whole from `store`: every row group, every column.
pub(crate) async fn verify_file(store: &Arc<dyn ObjectStore>, file: &FileMeta) -> Verdict {
    crate::file_format::verify_parquet_file(store, file).await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::error::ArrowError;
    use bytes::Bytes;
    use datafusion::error::DataFusionError;
    use object_store::path::Path as ObjectPath;
    use object_store::{ObjectStoreExt, PutPayload};
    use parquet::errors::ParquetError;
    use uuid::Uuid;

    use super::*;
    use crate::catalog::FileTable;

    fn store_error() -> object_store::Error {
        object_store::Error::Generic {
            store: "test",
            source: Box::new(std::io::Error::other("unreachable")),
        }
    }

    fn missing() -> object_store::Error {
        object_store::Error::NotFound {
            path: "tables/spans/x".to_owned(),
            source: Box::new(std::io::Error::other("missing")),
        }
    }

    fn parquet_error() -> ParquetError {
        ParquetError::General("bad page".to_owned())
    }

    fn file_id(last: u128) -> FileId {
        FileId(Uuid::from_u128(last))
    }

    #[test]
    fn a_parquet_error_is_corruption_unless_the_store_failed() {
        assert!(may_be_corrupt(&DataFusionError::ParquetError(Box::new(
            parquet_error()
        ))));
        assert!(!may_be_corrupt(&DataFusionError::ParquetError(Box::new(
            ParquetError::External(Box::new(store_error()))
        ))));
        assert!(may_be_corrupt(&DataFusionError::ParquetError(Box::new(
            ParquetError::External(Box::new(missing()))
        ))));
    }

    #[test]
    fn an_arrow_wrapped_parquet_error_is_corruption() {
        let error = DataFusionError::ArrowError(
            Box::new(ArrowError::ParquetError("bad page".to_owned())),
            None,
        );
        assert!(may_be_corrupt(&error));
        let other = DataFusionError::ArrowError(Box::new(ArrowError::DivideByZero), None);
        assert!(!may_be_corrupt(&other));
    }

    #[test]
    fn an_external_parquet_or_arrow_error_is_corruption() {
        let parquet = DataFusionError::External(Box::new(parquet_error()));
        assert!(may_be_corrupt(&parquet));
        let arrow =
            DataFusionError::External(Box::new(ArrowError::ParquetError("bad page".to_owned())));
        assert!(may_be_corrupt(&arrow));
        let store =
            DataFusionError::External(Box::new(ParquetError::External(Box::new(store_error()))));
        assert!(!may_be_corrupt(&store));
    }

    #[test]
    fn wrappers_are_looked_through() {
        let inner = DataFusionError::ParquetError(Box::new(parquet_error()));
        let context = DataFusionError::Context("reading".to_owned(), Box::new(inner));
        let shared = DataFusionError::Shared(Arc::new(context));
        assert!(may_be_corrupt(&shared));
        let store = DataFusionError::Shared(Arc::new(DataFusionError::Context(
            "reading".to_owned(),
            Box::new(DataFusionError::ParquetError(Box::new(
                ParquetError::External(Box::new(store_error())),
            ))),
        )));
        assert!(!may_be_corrupt(&store));
    }

    #[test]
    fn other_errors_are_not_corruption() {
        assert!(!may_be_corrupt(&DataFusionError::ObjectStore(Box::new(
            store_error()
        ))));
        assert!(!may_be_corrupt(&DataFusionError::ResourcesExhausted(
            "full".to_owned()
        )));
        assert!(!may_be_corrupt(&DataFusionError::Plan("bad".to_owned())));
    }

    #[test]
    fn a_store_error_is_not_corruption_but_a_missing_object_is_a_suspect() {
        assert!(is_store_error(&ParquetError::External(Box::new(
            store_error()
        ))));
        assert!(!is_store_error(&ParquetError::External(
            Box::new(missing())
        )));
        assert!(!is_store_error(&parquet_error()));
    }

    #[test]
    fn suspects_wait_once_and_healthy_files_are_skipped_for_an_hour() {
        let queue = VerifyQueue::default();
        let now = 10 * HEALTHY_FOR;
        assert_eq!(queue.suspect([file_id(1), file_id(2), file_id(1)], now), 2);
        assert_eq!(queue.suspect([file_id(1)], now), 0);
        assert_eq!(queue.take(1), vec![file_id(1)]);
        // Taken, so a new suspect queues it again.
        assert_eq!(queue.suspect([file_id(1)], now), 1);
        assert_eq!(queue.take(10), vec![file_id(2), file_id(1)]);

        queue.healthy(file_id(3), now);
        assert_eq!(queue.suspect([file_id(3)], now + HEALTHY_FOR - 1), 0);
        assert_eq!(queue.suspect([file_id(3)], now + HEALTHY_FOR), 1);
    }

    #[test]
    fn a_requeued_file_goes_to_the_end() {
        let queue = VerifyQueue::default();
        queue.suspect([file_id(1), file_id(2)], 0);
        let first = queue.take(1);
        queue.requeue(first[0]);
        queue.requeue(first[0]);
        assert_eq!(queue.take(10), vec![file_id(2), file_id(1)]);
    }

    fn recorded_file(id: FileId) -> Arc<FileMeta> {
        Arc::new(FileMeta {
            id,
            table: FileTable::Spans,
            project: crate::compaction::job::tests::project(),
            day: chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
            level: 0,
            path: format!("tables/spans/day=2026-01-01/{id:?}.parquet"),
            size: 1,
            footer_size: 1,
            rows: 1,
            min_time: 0,
            max_time: 0,
            min_ingest_time: 0,
            max_ingest_time: 0,
            fully_merged: false,
            deletions_applied_seq: 0,
        })
    }

    #[test]
    fn a_report_queues_the_recorded_files_only_for_a_decode_error() {
        let queue = Arc::new(VerifyQueue::default());
        let suspects = Suspects::new(Arc::clone(&queue));
        suspects.record(&[recorded_file(file_id(7)), recorded_file(file_id(8))]);

        suspects.report(&DataFusionError::ResourcesExhausted("full".to_owned()));
        assert!(queue.take(10).is_empty());

        suspects.report(&DataFusionError::ParquetError(Box::new(parquet_error())));
        // The recorded files come from a set, so their order is not fixed.
        let mut queued = queue.take(10);
        queued.sort();
        assert_eq!(queued, vec![file_id(7), file_id(8)]);
    }

    #[test]
    fn a_full_queue_drops_new_suspects() {
        let queue = VerifyQueue::default();
        let ids = (0..QUEUE_MAX as u128 + 5).map(|n| file_id(n + 1));
        assert_eq!(queue.suspect(ids, 0), QUEUE_MAX);
        assert_eq!(queue.take(usize::MAX).len(), QUEUE_MAX);
    }

    /// The committed `parquet_file` regression input (21 bytes, read as the fuzz
    /// entry reads it): table byte 0x19 (traces), footer hint 0x00ffffff, a time
    /// bound, then the file. The file is the 8 bytes `2f 15 00 00 "PAR1"`, whose
    /// footer claims 0x152f bytes of metadata. Upstream's push decoder underflows
    /// on it (parquet `push_decoder.rs:391`). The guard turns that panic into a
    /// decode failure, so every read reports the file corrupt.
    #[test]
    fn a_footer_claiming_more_metadata_than_the_file_is_corrupt() {
        const DAY_NS: i64 = 86_400_000_000_000;
        const REGRESSION: &[u8] = include_bytes!(
            "../../../fuzz/regressions/parquet_file/min-crash-3c4803703fcac6d6394de27518277ee19c29540a"
        );
        assert_eq!(REGRESSION.len(), 21);
        let footer_hint = u64::from(u32::from_le_bytes(REGRESSION[1..5].try_into().unwrap()));
        let file = Bytes::copy_from_slice(&REGRESSION[13..]);
        assert_eq!(
            &file[..4],
            &0x152f_u32.to_le_bytes(),
            "the footer claims 0x152f bytes"
        );
        let size = u64::try_from(file.len()).unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
        let runtime = crate::fuzz_entry::runtime();

        let traces = crate::fuzz_entry::parquet_meta(FileTable::Traces, size, footer_hint);
        runtime.block_on(async {
            store
                .put(
                    &ObjectPath::from(traces.path.as_str()),
                    PutPayload::from(file.clone()),
                )
                .await
                .unwrap();
            let verdict = verify_file(&store, &traces).await;
            assert!(
                matches!(
                    verdict,
                    Verdict::Corrupt {
                        reason: "decode failed"
                    }
                ),
                "the footer is reported corrupt"
            );
            let mut anchors = crate::anchor::Anchors::new(DAY_NS);
            assert!(
                anchors
                    .rebuild_file(&traces, Arc::clone(&store), 0)
                    .await
                    .is_err()
            );
        });

        let spans = crate::fuzz_entry::parquet_meta(FileTable::Spans, size, footer_hint);
        runtime.block_on(async {
            store
                .put(
                    &ObjectPath::from(spans.path.as_str()),
                    PutPayload::from(file),
                )
                .await
                .unwrap();
            let files = [Arc::new(spans)];
            let read = crate::compaction::slice::time_values(
                Arc::clone(&store),
                &files,
                cetra_core::schema::spans::START_TIME,
            )
            .await;
            assert!(read.is_err(), "the start times do not read");
        });
    }
}
