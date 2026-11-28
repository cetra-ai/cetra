//! `cetra_statam::RangeReader` adapter over [`StorageIo`]: direct local `pread`
//! via [`cetra_statam::FileReader`] on local disks and cancellation-aware ranged
//! `ObjectStore` reads on remote/test stores.

use std::fmt;
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use bytes::Bytes;
use cetra_column::CancelToken;
use cetra_statam::{FileReader, RangeReader, StatamError};
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt};
use tokio::runtime::RuntimeFlavor;

use crate::objects::StorageIo;

/// Runs a synchronous blocking closure that may use [`open_range_reader`] against
/// `runtime`, ensuring it never runs inside a `CurrentThread` Tokio runtime context.
pub(crate) async fn run_blocking_io<T, F>(runtime: tokio::runtime::Handle, f: F) -> Result<T, ()>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    if runtime.runtime_flavor() == RuntimeFlavor::MultiThread {
        tokio::task::spawn_blocking(f).await.map_err(|_| ())
    } else {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if std::thread::Builder::new()
            .name("statam-blocking-io".to_owned())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(f));
                let _ = tx.send(result);
            })
            .is_err()
        {
            return Err(());
        }
        match rx.await {
            Ok(Ok(val)) => Ok(val),
            Ok(Err(_)) | Err(_) => Err(()),
        }
    }
}

/// Opens a [`RangeReader`] for `key` over `io`, choosing direct local `pread`
/// when `io.local_root()` is configured and an async-bridged [`ObjectStoreRangeReader`]
/// otherwise.
pub(crate) fn open_range_reader(
    io: &StorageIo,
    key: &str,
    expected_len: u64,
    runtime: tokio::runtime::Handle,
    cancel: CancelToken,
) -> Result<Arc<dyn RangeReader>, StatamError> {
    if cancel.is_cancelled() {
        return Err(StatamError::Cancelled);
    }
    if let Some(root) = io.local_root() {
        let reader = FileReader::open(root.join(key))?;
        return Ok(Arc::new(LocalRangeReader {
            inner: reader,
            cancel,
        }));
    }
    let path = ObjectPath::parse(key).map_err(|_| StatamError::Corrupt {
        reason: "invalid object key",
    })?;
    Ok(Arc::new(ObjectStoreRangeReader {
        store: Arc::clone(io.store()),
        path,
        file_len: expected_len,
        runtime,
        is_remote: io.is_remote(),
        cancel,
    }))
}

/// Direct local-disk [`RangeReader`] wrapping [`FileReader`] with cooperative cancellation.
struct LocalRangeReader {
    inner: FileReader,
    cancel: CancelToken,
}

impl fmt::Debug for LocalRangeReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalRangeReader")
            .field("file_len", &self.inner.file_len())
            .finish()
    }
}

impl RangeReader for LocalRangeReader {
    fn file_len(&self) -> u64 {
        self.inner.file_len()
    }

    fn read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError> {
        if self.cancel.is_cancelled() {
            return Err(StatamError::Cancelled);
        }
        self.inner.read_range(range)
    }

    fn read_ranges(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, StatamError> {
        if self.cancel.is_cancelled() {
            return Err(StatamError::Cancelled);
        }
        self.inner.read_ranges(ranges)
    }

    fn is_remote(&self) -> bool {
        false
    }
}

/// Cancellation-aware [`RangeReader`] backed by an async [`ObjectStore`].
struct ObjectStoreRangeReader {
    store: Arc<dyn ObjectStore>,
    path: ObjectPath,
    file_len: u64,
    runtime: tokio::runtime::Handle,
    is_remote: bool,
    cancel: CancelToken,
}

impl fmt::Debug for ObjectStoreRangeReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObjectStoreRangeReader")
            .field("file_len", &self.file_len)
            .field("is_remote", &self.is_remote)
            .finish_non_exhaustive()
    }
}

impl ObjectStoreRangeReader {
    fn validate_range(&self, range: &Range<u64>) -> Result<(), StatamError> {
        if range.start > range.end || range.end > self.file_len {
            return Err(StatamError::Corrupt {
                reason: "byte range out of object bounds",
            });
        }
        Ok(())
    }

    fn fetch_ranges_sync(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, StatamError> {
        for range in ranges {
            self.validate_range(range)?;
        }
        if ranges.is_empty() {
            return Ok(Vec::new());
        }
        if self.cancel.is_cancelled() {
            return Err(StatamError::Cancelled);
        }

        // Track empty ranges separately so every request sent to `object_store` is non-empty.
        let mut non_empty_indices = Vec::with_capacity(ranges.len());
        let mut non_empty_ranges = Vec::with_capacity(ranges.len());
        for (idx, range) in ranges.iter().enumerate() {
            if range.start < range.end {
                non_empty_indices.push(idx);
                non_empty_ranges.push(range.clone());
            }
        }
        if non_empty_ranges.is_empty() {
            return Ok(vec![Bytes::new(); ranges.len()]);
        }

        // Calling `Handle::block_on` on a `CurrentThread` runtime from inside a Tokio runtime
        // context would panic/deadlock. Reject it explicitly; `WorkerPool` threads are outside
        // Tokio, and `MultiThread` blocking threads safely execute `block_on`.
        if tokio::runtime::Handle::try_current().is_ok()
            && self.runtime.runtime_flavor() == RuntimeFlavor::CurrentThread
        {
            return Err(StatamError::Store {
                reason: "cannot block on current_thread runtime",
            });
        }

        let store = Arc::clone(&self.store);
        let path = self.path.clone();
        let file_len = self.file_len;
        let cancel = self.cancel.clone();
        let runtime = self.runtime.clone();
        let expected_lens: Vec<u64> = non_empty_ranges.iter().map(|r| r.end - r.start).collect();

        let caught = catch_unwind(AssertUnwindSafe(move || {
            runtime.block_on(fetch_ranges_async(
                store,
                path,
                non_empty_ranges,
                file_len,
                cancel,
            ))
        }));

        let fetched = match caught {
            Ok(res) => res?,
            Err(_) => {
                return Err(StatamError::Corrupt {
                    reason: "decoder panicked",
                });
            }
        };

        if fetched.len() != expected_lens.len() {
            return Err(StatamError::Corrupt {
                reason: "missing or truncated object",
            });
        }
        let mut out = vec![Bytes::new(); ranges.len()];
        for ((slot, bytes), expected) in non_empty_indices
            .into_iter()
            .zip(fetched)
            .zip(expected_lens)
        {
            if bytes.len() as u64 != expected {
                return Err(StatamError::Corrupt {
                    reason: "missing or truncated object",
                });
            }
            out[slot] = bytes;
        }
        Ok(out)
    }
}

impl RangeReader for ObjectStoreRangeReader {
    fn file_len(&self) -> u64 {
        self.file_len
    }

    fn read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError> {
        let mut bufs = self.fetch_ranges_sync(&[range])?;
        bufs.pop().ok_or(StatamError::Corrupt {
            reason: "missing or truncated object",
        })
    }

    fn read_ranges(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, StatamError> {
        self.fetch_ranges_sync(ranges)
    }

    fn is_remote(&self) -> bool {
        self.is_remote && self.runtime.runtime_flavor() == RuntimeFlavor::MultiThread
    }
}

async fn fetch_ranges_async(
    store: Arc<dyn ObjectStore>,
    path: ObjectPath,
    ranges: Vec<Range<u64>>,
    expected_file_size: u64,
    cancel: CancelToken,
) -> Result<Vec<Bytes>, StatamError> {
    let notify = Arc::new(tokio::sync::Notify::new());
    let _cancel_guard = cancel.register_waker({
        let notify = Arc::clone(&notify);
        move || notify.notify_one()
    });
    if cancel.is_cancelled() {
        return Err(StatamError::Cancelled);
    }

    let res = {
        let fut = store.get_ranges(&path, &ranges);
        tokio::pin!(fut);
        loop {
            tokio::select! {
                biased;
                _ = notify.notified() => {
                    if cancel.is_cancelled() {
                        return Err(StatamError::Cancelled);
                    }
                }
                res = &mut fut => break res,
            }
        }
    };

    match res {
        Ok(bufs) => Ok(bufs),
        Err(err) => {
            if crate::verify::is_corrupt_store_error(&err) {
                return Err(StatamError::Corrupt {
                    reason: "missing or truncated object",
                });
            }
            if matches!(
                &err,
                object_store::Error::Generic { store: name, .. }
                    if *name == "LocalFileSystem" || *name == "InMemory"
            ) {
                let head_fut = store.head(&path);
                tokio::pin!(head_fut);
                let head_res = loop {
                    tokio::select! {
                        biased;
                        _ = notify.notified() => {
                            if cancel.is_cancelled() {
                                return Err(StatamError::Cancelled);
                            }
                        }
                        res = &mut head_fut => break res,
                    }
                };
                match head_res {
                    Ok(meta) if meta.size < expected_file_size => {
                        return Err(StatamError::Corrupt {
                            reason: "missing or truncated object",
                        });
                    }
                    Err(head_err) if crate::verify::is_corrupt_store_error(&head_err) => {
                        return Err(StatamError::Corrupt {
                            reason: "missing or truncated object",
                        });
                    }
                    _ => {}
                }
            }
            Err(StatamError::Store {
                reason: "object store read failed",
            })
        }
    }
}
