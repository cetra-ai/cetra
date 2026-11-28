//! The error boundary around Statam decoding (spec 03 §10): a
//! decoder panic becomes a decode error, so the read fails, its files become
//! suspects, and verification can quarantine them.

use std::panic::{AssertUnwindSafe, catch_unwind};

use cetra_statam::StatamError;

/// The error a caught decoder panic becomes.
pub(crate) fn decoder_panicked() -> StatamError {
    StatamError::Corrupt {
        reason: "decoder panicked",
    }
}

/// Runs `f`, turning a panic into `Err(decoder_panicked())`.
pub(crate) fn guard_sync<T>(f: impl FnOnce() -> Result<T, StatamError>) -> Result<T, StatamError> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(decoder_panicked()))
}

#[cfg(test)]
mod tests {
    use std::fmt;
    use std::sync::Arc;

    use async_trait::async_trait;
    use cetra_column::Batch;
    use cetra_core::schema::{scores, spans};
    use futures::stream::BoxStream;
    use object_store::memory::InMemory;
    use object_store::path::Path;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
        ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
    };
    use uuid::Uuid;

    use super::*;
    use crate::anchor::Anchors;
    use crate::catalog::{FileId, FileMeta, FileTable};
    use crate::compaction::slice::time_values;
    use crate::error::{AnchorError, CompactionError};
    use crate::fuzz_entry::tests::parquet_bytes;
    use crate::fuzz_entry::{parquet_meta, parquet_query, runtime};
    use crate::objects::StorageIo;
    use crate::verify::{Verdict, may_be_corrupt, verify_file};
    use crate::{ListScope, SortOrder, SpanListRequest, SpanSort, span_list};

    const DAY_NS: i64 = 86_400 * 1_000_000_000;

    /// A store whose range reads panic, as a decoder panic does inside a scan's poll.
    /// `head` and full reads pass through, so the panic happens at the footer read.
    #[derive(Debug, Default)]
    struct PanicStore {
        inner: InMemory,
    }

    impl fmt::Display for PanicStore {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "PanicStore")
        }
    }

    #[async_trait]
    impl ObjectStore for PanicStore {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            opts: PutOptions,
        ) -> object_store::Result<PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &Path,
            opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &Path,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            if options.range.is_some() {
                panic!("test store panics on read");
            }
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &Path,
            to: &Path,
            options: CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    /// Puts one written file of `table` into `store` at its fixed path, and returns its catalog row.
    fn stored(table: FileTable, store: &Arc<dyn ObjectStore>) -> FileMeta {
        let written = parquet_bytes(table);
        let mut meta = parquet_meta(table, written.bytes.len() as u64, written.footer_size);
        meta.rows = written.rows;
        runtime().block_on(async {
            store
                .put(
                    &Path::from(meta.path.as_str()),
                    PutPayload::from(written.bytes.clone()),
                )
                .await
                .expect("store the test file")
        });
        meta
    }

    /// Every span of the seed file, with the default exclusions off.
    fn all_spans() -> SpanListRequest<'static> {
        SpanListRequest {
            strict_exclusions: false,
            filter: None,
            search: None,
            scope: ListScope {
                window: None,
                include_eval: true,
                include_experiments: true,
            },
            sort: SpanSort::StartTime,
            order: SortOrder::Desc,
            limit: 100,
            cursor: None,
            timeout: std::time::Duration::from_secs(30),
        }
    }

    #[test]
    fn guard_sync_turns_a_panic_into_an_error() {
        let result: Result<(), StatamError> =
            guard_sync(|| -> Result<(), StatamError> { panic!("boom") });
        assert!(matches!(result, Err(ref error) if error.to_string().contains("decoder panicked")));
    }

    #[test]
    fn a_panicking_scan_fails_the_query_and_suspects_its_file() {
        let store: Arc<dyn ObjectStore> = Arc::new(PanicStore::default());
        let meta = stored(FileTable::Spans, &store);
        let (ctx, verify) = parquet_query(store, &meta);
        let result = runtime().block_on(span_list(&ctx, all_spans()));
        assert!(result.is_err(), "a panicking read must fail the query");
        assert_eq!(verify.take(10), vec![FileId(Uuid::from_u128(1))]);
    }

    #[test]
    fn the_runtime_is_usable_after_a_caught_panic() {
        let store: Arc<dyn ObjectStore> = Arc::new(PanicStore::default());
        let meta = stored(FileTable::Spans, &store);
        let (panicking, _) = parquet_query(store, &meta);
        let _ = runtime().block_on(span_list(&panicking, all_spans()));

        let plain: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let meta = stored(FileTable::Spans, &plain);
        let (ctx, _) = parquet_query(plain, &meta);
        let page = runtime()
            .block_on(span_list(&ctx, all_spans()))
            .expect("a plain read works after the panic");
        let rows: usize = page.batches.iter().map(Batch::num_rows).sum();
        assert_eq!(rows, crate::test_support::dataset_spans().len());
    }

    #[test]
    fn verify_reports_a_panicking_read_as_corrupt() {
        let store: Arc<dyn ObjectStore> = Arc::new(PanicStore::default());
        let meta = stored(FileTable::Spans, &store);
        let io = StorageIo::from_store(store);
        let verdict = runtime().block_on(verify_file(&io, &meta));
        assert_eq!(
            verdict,
            Verdict::Corrupt {
                reason: "decode failed"
            }
        );
    }

    #[test]
    fn anchor_rebuild_skips_a_panicking_file() {
        let store: Arc<dyn ObjectStore> = Arc::new(PanicStore::default());
        let meta = stored(FileTable::Traces, &store);
        let io = StorageIo::from_store(store);
        let mut anchors = Anchors::new(DAY_NS);
        let result = runtime().block_on(anchors.rebuild_file(&meta, io, 0));
        assert!(
            matches!(result, Err(AnchorError::Statam(ref error)) if !error.is_store_error()),
            "expected a decode error, got {result:?}"
        );
    }

    #[test]
    fn time_values_reports_a_panic_as_a_corrupt_read() {
        let store: Arc<dyn ObjectStore> = Arc::new(PanicStore::default());
        let meta = stored(FileTable::Spans, &store);
        let io = StorageIo::from_store(Arc::clone(&store));
        let result = runtime().block_on(time_values(io, &[Arc::new(meta)], spans::START_TIME));
        match result {
            Err(CompactionError::Read { source }) => assert!(may_be_corrupt(&source)),
            other => panic!("expected a read error, got {other:?}"),
        }
        // The scores column reads through the same guard.
        let meta = stored(FileTable::Scores, &store);
        let io = StorageIo::from_store(store);
        let result = runtime().block_on(time_values(io, &[Arc::new(meta)], scores::CREATED_AT));
        assert!(matches!(result, Err(CompactionError::Read { .. })));
    }
}
