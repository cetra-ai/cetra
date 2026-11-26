//! Out-of-core temporary spill files encoded as LZ4+CRC-32C `cetra_column::frame` batches.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cetra_column::frame::{FRAME_HEADER_LEN, MAX_PAYLOAD, decode_batch, encode_batch};
use cetra_column::{Batch, ColumnError, Reservation, Schema};

use crate::error::QueryError;

/// Default maximum simultaneously open spill files during a k-way merge pass.
pub const MAX_MERGE_WAY: usize = 32;

/// Callback returning the frozen process-wide maximum total spilled bytes allowed right now
/// (wired to `Disk::engine_spill_cap()` in `cetra-storage`).
pub type SpillCapFn = Arc<dyn Fn() -> u64 + Send + Sync>;

fn map_spill_io_err(err: &io::Error) -> QueryError {
    match err.kind() {
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => QueryError::TooLarge,
        _ => QueryError::internal(format!("spill I/O error: {err}")),
    }
}

fn reserve_spill_bytes(
    spilled_bytes: &AtomicU64,
    cap: &SpillCapFn,
    add: u64,
) -> Result<(), QueryError> {
    if add == 0 {
        return Ok(());
    }
    let limit = cap();
    let mut cur = spilled_bytes.load(Ordering::Acquire);
    loop {
        let Some(next) = cur.checked_add(add).filter(|&n| n <= limit) else {
            return Err(QueryError::TooLarge);
        };
        match spilled_bytes.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Ok(()),
            Err(actual) => cur = actual,
        }
    }
}

/// Manages unique temporary spill files inside a root directory (`<data_dir>/tmp/query`).
/// Implements manual `fmt::Debug` (counters only).
#[derive(Clone)]
pub struct SpillManager {
    root: PathBuf,
    next_id: Arc<AtomicU64>,
    spilled_bytes: Arc<AtomicU64>,
    cap: SpillCapFn,
}

impl fmt::Debug for SpillManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpillManager")
            .field("spilled_bytes", &self.spilled_bytes())
            .field("next_id", &self.next_id.load(Ordering::Relaxed))
            .finish()
    }
}

impl SpillManager {
    /// Creates a spill manager rooted at `dir` with a dynamic byte cap callback.
    pub fn new(dir: PathBuf, cap: SpillCapFn) -> Self {
        Self {
            root: dir,
            next_id: Arc::new(AtomicU64::new(0)),
            spilled_bytes: Arc::new(AtomicU64::new(0)),
            cap,
        }
    }

    /// Convenience constructor with a fixed byte cap (for unit tests).
    pub fn with_fixed_cap(dir: PathBuf, max_bytes: u64) -> Self {
        Self::new(dir, Arc::new(move || max_bytes))
    }

    /// Shared live spilled-bytes counter (passed to `Disk` so `apply_spill_cap` reads exact live bytes).
    pub fn spilled_bytes_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.spilled_bytes)
    }

    /// Current live spilled bytes across all active spill writers and files.
    pub fn spilled_bytes(&self) -> u64 {
        self.spilled_bytes.load(Ordering::Acquire)
    }

    /// Creates a lazy spill appender inside `dir` (0 FDs and 0 files on disk until first `append_batch`).
    pub fn create_appender(&self) -> SpillAppender {
        SpillAppender {
            created: false,
            poisoned: false,
            inner: None,
            root: self.root.clone(),
            next_id: Arc::clone(&self.next_id),
            spilled_bytes: Arc::clone(&self.spilled_bytes),
            cap: Arc::clone(&self.cap),
            scratch_buf: Vec::new(),
        }
    }

    /// Opens a new unique spill writer inside `dir` (deleted + bytes subtracted on drop if uncommitted).
    pub fn create_file(&self) -> Result<SpillWriter, QueryError> {
        fs::create_dir_all(&self.root).map_err(|e| map_spill_io_err(&e))?;
        let pid = std::process::id();
        loop {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let path = self.root.join(format!("spill-{pid}-{id}.cbm"));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(SpillWriter {
                        path,
                        file: Some(file),
                        bytes_written: 0,
                        spilled_bytes: Arc::clone(&self.spilled_bytes),
                        cap: Arc::clone(&self.cap),
                        committed: false,
                        scratch_buf: Vec::new(),
                    });
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(map_spill_io_err(&err)),
            }
        }
    }
}

/// Streaming writer appending `cetra_column::Batch`es as LZ4+CRC-32C frames (`cetra_column::frame`).
pub struct SpillWriter {
    path: PathBuf,
    file: Option<File>,
    bytes_written: u64,
    spilled_bytes: Arc<AtomicU64>,
    cap: SpillCapFn,
    committed: bool,
    scratch_buf: Vec<u8>,
}

impl fmt::Debug for SpillWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpillWriter")
            .field("bytes_written", &self.bytes_written)
            .field("committed", &self.committed)
            .finish()
    }
}

impl SpillWriter {
    /// Writes `batch` (compacting `Selection` if non-`All`), checking `spilled_bytes + frame_len <= (cap)()`
    /// before writing. Maps `StorageFull`/`QuotaExceeded`/cap-exceeded to `QueryError::TooLarge`
    /// and other I/O errors to `QueryError::Internal(...)`.
    pub fn write_batch(&mut self, batch: &Batch) -> Result<(), QueryError> {
        self.scratch_buf.clear();
        encode_batch(batch, &mut self.scratch_buf, None)?;
        let frame_len = u64::try_from(self.scratch_buf.len()).map_err(|_| QueryError::TooLarge)?;
        reserve_spill_bytes(&self.spilled_bytes, &self.cap, frame_len)?;

        let Some(file) = self.file.as_mut() else {
            self.spilled_bytes.fetch_sub(frame_len, Ordering::AcqRel);
            return Err(QueryError::internal("spill writer already closed"));
        };

        if let Err(err) = file.write_all(&self.scratch_buf) {
            self.spilled_bytes.fetch_sub(frame_len, Ordering::AcqRel);
            return Err(map_spill_io_err(&err));
        }

        self.bytes_written = self.bytes_written.saturating_add(frame_len);
        Ok(())
    }

    /// Flushes and converts into a readable [`SpillFile`] (auto-deleted on `SpillFile::drop`).
    pub fn finish(mut self) -> Result<SpillFile, QueryError> {
        if let Some(mut file) = self.file.take() {
            file.flush().map_err(|e| map_spill_io_err(&e))?;
        }
        self.committed = true;
        Ok(SpillFile {
            inner: Arc::new(SpillFileInner {
                path: std::mem::take(&mut self.path),
                byte_len: AtomicU64::new(self.bytes_written),
                spilled_bytes: Arc::clone(&self.spilled_bytes),
            }),
        })
    }
}

impl Drop for SpillWriter {
    fn drop(&mut self) {
        if !self.committed {
            drop(self.file.take());
            let _ = fs::remove_file(&self.path);
            if self.bytes_written > 0 {
                self.spilled_bytes
                    .fetch_sub(self.bytes_written, Ordering::AcqRel);
            }
        }
    }
}

struct SpillFileInner {
    path: PathBuf,
    byte_len: AtomicU64,
    spilled_bytes: Arc<AtomicU64>,
}

impl Drop for SpillFileInner {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let len = self.byte_len.load(Ordering::Relaxed);
        if len > 0 {
            self.spilled_bytes.fetch_sub(len, Ordering::AcqRel);
        }
    }
}

/// Lazy, zero-idle-FD streaming appender writing `cetra_column::Batch` frames on demand.
///
/// Holds no file descriptor between `append_batch` calls and creates no file on disk if unused.
pub struct SpillAppender {
    created: bool,
    poisoned: bool,
    inner: Option<Arc<SpillFileInner>>,
    root: PathBuf,
    next_id: Arc<AtomicU64>,
    spilled_bytes: Arc<AtomicU64>,
    cap: SpillCapFn,
    scratch_buf: Vec<u8>,
}

impl fmt::Debug for SpillAppender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpillAppender")
            .field("created", &self.created)
            .field("poisoned", &self.poisoned)
            .field("byte_len", &self.byte_len())
            .finish()
    }
}

impl SpillAppender {
    /// Bytes written so far to this appender.
    pub fn byte_len(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |inner| inner.byte_len.load(Ordering::Acquire))
    }

    /// Encodes and appends `batch` as one LZ4+CRC-32C frame, opening the file only for the duration
    /// of the write (0 idle FDs). Marks `self` poisoned on any partial write error.
    pub fn append_batch(
        &mut self,
        batch: &Batch,
        pool: Option<&mut Reservation>,
    ) -> Result<(), QueryError> {
        if self.poisoned {
            return Err(QueryError::internal(
                "spill appender is poisoned after write failure",
            ));
        }
        self.scratch_buf.clear();
        encode_batch(batch, &mut self.scratch_buf, pool)?;
        let frame_len = u64::try_from(self.scratch_buf.len()).map_err(|_| QueryError::TooLarge)?;
        reserve_spill_bytes(&self.spilled_bytes, &self.cap, frame_len)?;

        let mut file = if !self.created {
            if let Err(err) = fs::create_dir_all(&self.root) {
                self.spilled_bytes.fetch_sub(frame_len, Ordering::AcqRel);
                return Err(map_spill_io_err(&err));
            }
            let pid = std::process::id();
            loop {
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                let path = self.root.join(format!("spill-{pid}-{id}.cbm"));
                match OpenOptions::new().write(true).create_new(true).open(&path) {
                    Ok(f) => {
                        self.inner = Some(Arc::new(SpillFileInner {
                            path,
                            byte_len: AtomicU64::new(0),
                            spilled_bytes: Arc::clone(&self.spilled_bytes),
                        }));
                        self.created = true;
                        break f;
                    }
                    Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                    Err(err) => {
                        self.spilled_bytes.fetch_sub(frame_len, Ordering::AcqRel);
                        return Err(map_spill_io_err(&err));
                    }
                }
            }
        } else {
            let Some(inner) = self.inner.as_ref() else {
                self.spilled_bytes.fetch_sub(frame_len, Ordering::AcqRel);
                return Err(QueryError::internal("spill appender missing inner state"));
            };
            match OpenOptions::new().append(true).open(&inner.path) {
                Ok(f) => f,
                Err(err) => {
                    self.spilled_bytes.fetch_sub(frame_len, Ordering::AcqRel);
                    return Err(map_spill_io_err(&err));
                }
            }
        };

        if let Err(err) = file
            .write_all(&self.scratch_buf)
            .and_then(|()| file.flush())
        {
            self.poisoned = true;
            self.spilled_bytes.fetch_sub(frame_len, Ordering::AcqRel);
            drop(file);
            return Err(map_spill_io_err(&err));
        }
        drop(file);

        if let Some(inner) = &self.inner {
            inner.byte_len.fetch_add(frame_len, Ordering::AcqRel);
        }
        Ok(())
    }

    /// Consumes this appender and returns `Some(SpillFile)` if any batch was written, `None` if unused
    /// (or poisoned).
    pub fn finish(mut self) -> Option<SpillFile> {
        if self.poisoned {
            self.inner = None;
            return None;
        }
        self.inner
            .take()
            .filter(|inner| inner.byte_len.load(Ordering::Acquire) > 0)
            .map(|inner| SpillFile { inner })
    }
}

/// Handle to a completed temporary spill file; deletes the file and subtracts its bytes from
/// `SpillManager::spilled_bytes()` when the last [`SpillFile`] and all [`SpillReader`]s drop.
pub struct SpillFile {
    inner: Arc<SpillFileInner>,
}

impl fmt::Debug for SpillFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpillFile")
            .field("byte_len", &self.byte_len())
            .finish()
    }
}

impl SpillFile {
    /// Bytes on disk.
    pub fn byte_len(&self) -> u64 {
        self.inner.byte_len.load(Ordering::Acquire)
    }

    /// Opens a sequential frame reader validating against `schema`.
    pub fn open_reader(&self, schema: Arc<Schema>) -> Result<SpillReader, QueryError> {
        let file = File::open(&self.inner.path).map_err(|e| map_spill_io_err(&e))?;
        Ok(SpillReader {
            reader: BufReader::with_capacity(64 * 1024, file),
            _inner: Arc::clone(&self.inner),
            schema,
            frame_buf: Vec::new(),
        })
    }
}

/// Sequential frame reader yielding `Option<Batch>` from a [`SpillFile`].
pub struct SpillReader {
    // Declared before `_inner` so the OS file descriptor closes before unlink & byte decrement.
    reader: BufReader<File>,
    _inner: Arc<SpillFileInner>,
    schema: Arc<Schema>,
    frame_buf: Vec<u8>,
}

impl fmt::Debug for SpillReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpillReader")
            .field("schema", &self.schema)
            .field("frame_buf_len", &self.frame_buf.len())
            .finish()
    }
}

impl SpillReader {
    /// Reads the next batch. `Reservation` postcondition: `res.size() == before + batch.memory_bytes()`
    /// on `Ok(Some(batch))`, and restored to `before` on `Ok(None)` or `Err`.
    pub fn next_batch(
        &mut self,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        let mut header = [0u8; FRAME_HEADER_LEN];
        let mut read_so_far = 0usize;
        while read_so_far < FRAME_HEADER_LEN {
            match self.reader.read(&mut header[read_so_far..]) {
                Ok(0) => {
                    if read_so_far == 0 {
                        rollback_res(res, before);
                        return Ok(None);
                    }
                    rollback_res(res, before);
                    return Err(ColumnError::InvalidFrame {
                        reason: "truncated spill frame header",
                    }
                    .into());
                }
                Ok(n) => read_so_far += n,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => {
                    rollback_res(res, before);
                    return Err(map_spill_io_err(&err));
                }
            }
        }

        let payload_len = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
        if payload_len > MAX_PAYLOAD {
            rollback_res(res, before);
            return Err(ColumnError::InvalidFrame {
                reason: "payload length exceeds MAX_PAYLOAD",
            }
            .into());
        }
        let Some(total_len) = FRAME_HEADER_LEN.checked_add(payload_len as usize) else {
            rollback_res(res, before);
            return Err(ColumnError::Overflow {
                reason: "spill frame length overflows usize",
            }
            .into());
        };

        self.frame_buf.resize(total_len, 0);
        self.frame_buf[..FRAME_HEADER_LEN].copy_from_slice(&header);
        if let Err(err) = self
            .reader
            .read_exact(&mut self.frame_buf[FRAME_HEADER_LEN..])
        {
            rollback_res(res, before);
            return if err.kind() == ErrorKind::UnexpectedEof {
                Err(ColumnError::InvalidFrame {
                    reason: "truncated spill frame payload",
                }
                .into())
            } else {
                Err(map_spill_io_err(&err))
            };
        }

        match decode_batch(&self.frame_buf, Some(&self.schema), res.as_deref_mut()) {
            Ok(batch) => Ok(Some(batch)),
            Err(err) => {
                rollback_res(res, before);
                Err(err.into())
            }
        }
    }
}

fn rollback_res(res: Option<&mut Reservation>, before: usize) {
    if let Some(r) = res {
        let cur = r.size();
        if cur > before {
            r.shrink(cur - before);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cetra_column::{
        Bitmap, Column, DataType, Field, FlatColumn, FlatData, GlobalPool, Selection, Utf8Data,
    };

    fn sample_batch(n: usize) -> Batch {
        let schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("text", DataType::Utf8, true),
        ])
        .unwrap();
        let ids: Vec<i64> = (0..n as i64).collect();
        let texts: Vec<&str> = (0..n)
            .map(|i| if i % 2 == 0 { "alpha" } else { "beta-payload" })
            .collect();
        let c0 = Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap());
        let c1 = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(texts).unwrap()),
                None,
            )
            .unwrap(),
        );
        Batch::try_new_all(schema, vec![c0, c1], n).unwrap()
    }

    #[test]
    fn spill_roundtrip_cap_enforcement_and_cleanup_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let spill = SpillManager::with_fixed_cap(dir.path().to_path_buf(), 1024 * 1024);

        // Uncommitted SpillWriter drop cleans up file and resets spilled_bytes to 0
        {
            let mut w = spill.create_file().unwrap();
            w.write_batch(&sample_batch(64)).unwrap();
            assert!(spill.spilled_bytes() > 0);
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        }
        assert_eq!(spill.spilled_bytes(), 0);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);

        // Committed SpillFile roundtrips filtered batch and enforces Reservation postcondition
        let full = sample_batch(16);
        let filtered = full
            .filter_mask(&Bitmap::from_bools(&[
                true, false, true, false, true, false, true, false, true, false, true, false, true,
                false, true, false,
            ]))
            .unwrap();
        let b2 = sample_batch(8);

        let spill_file = {
            let mut w = spill.create_file().unwrap();
            w.write_batch(&filtered).unwrap();
            w.write_batch(&b2).unwrap();
            w.finish().unwrap()
        };
        assert_eq!(spill.spilled_bytes(), spill_file.byte_len());
        assert!(spill_file.byte_len() > 0);

        let scope = GlobalPool::standalone(1024 * 1024);
        let mut res = scope.register("reader", true);
        res.try_grow(128).unwrap();
        let before = res.size();

        let mut reader = spill_file.open_reader(Arc::clone(full.schema())).unwrap();
        let got1 = reader.next_batch(Some(&mut res)).unwrap().unwrap();
        assert_eq!(res.size(), before + got1.memory_bytes());
        assert_eq!(got1.selection(), &Selection::All);
        assert!(got1.semantic_eq(&filtered));

        let before2 = res.size();
        let got2 = reader.next_batch(Some(&mut res)).unwrap().unwrap();
        assert_eq!(res.size(), before2 + got2.memory_bytes());
        assert!(got2.semantic_eq(&b2));

        let before_eof = res.size();
        assert!(reader.next_batch(Some(&mut res)).unwrap().is_none());
        assert_eq!(res.size(), before_eof);

        // Dropping SpillFile deletes file and resets spilled_bytes to 0
        drop(reader);
        drop(spill_file);
        assert_eq!(spill.spilled_bytes(), 0);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);

        // Tight reservation error on SpillReader restores Reservation to before
        let sf_small = {
            let mut w = spill.create_file().unwrap();
            w.write_batch(&b2).unwrap();
            w.finish().unwrap()
        };
        let tiny_scope = GlobalPool::standalone(100);
        let mut tiny_res = tiny_scope.register("tiny", true);
        tiny_res.try_grow(32).unwrap();
        let mut r_tiny = sf_small.open_reader(Arc::clone(full.schema())).unwrap();
        let err = r_tiny.next_batch(Some(&mut tiny_res)).unwrap_err();
        assert!(matches!(err, QueryError::TooLarge));
        assert_eq!(tiny_res.size(), 32);
        drop(r_tiny);
        drop(sf_small);
        assert_eq!(spill.spilled_bytes(), 0);

        // Fixed cap overflow maps to QueryError::TooLarge and cleans up
        let capped = SpillManager::with_fixed_cap(dir.path().to_path_buf(), 50);
        let mut w_cap = capped.create_file().unwrap();
        let err = w_cap.write_batch(&sample_batch(32)).unwrap_err();
        assert!(matches!(err, QueryError::TooLarge));
        assert_eq!(capped.spilled_bytes(), 0);
        drop(w_cap);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn spill_file_and_reader_debug_redact_paths_and_payload_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let spill = SpillManager::with_fixed_cap(dir.path().to_path_buf(), 1024 * 1024);
        let schema =
            Schema::try_new_arc(vec![Field::new("prompt", DataType::Utf8, false)]).unwrap();
        let secret = "super-secret-prompt-payload-xyz987";
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices([secret]).unwrap()),
                    None,
                )
                .unwrap(),
            )],
            1,
        )
        .unwrap();

        let sf = {
            let mut w = spill.create_file().unwrap();
            w.write_batch(&batch).unwrap();
            w.finish().unwrap()
        };
        let sf_dbg = format!("{sf:?}");
        assert!(sf_dbg.contains("byte_len"), "{sf_dbg}");
        assert!(!sf_dbg.contains(secret), "leaked payload in {sf_dbg}");
        let dir_str = dir.path().to_string_lossy();
        assert!(
            !sf_dbg.contains(dir_str.as_ref()),
            "leaked path in {sf_dbg}"
        );

        let mut reader = sf.open_reader(Arc::clone(&schema)).unwrap();
        let got = reader.next_batch(None).unwrap().unwrap();
        assert_eq!(got.num_rows(), 1);
        let reader_dbg = format!("{reader:?}");
        assert!(reader_dbg.contains("frame_buf_len"), "{reader_dbg}");
        assert!(
            !reader_dbg.contains(secret),
            "leaked payload in {reader_dbg}"
        );
        // Ensure raw frame_buf byte vector is not printed (e.g. uncompressed/LZ4 literals)
        assert!(
            !reader_dbg.contains("frame_buf:"),
            "leaked raw frame_buf in {reader_dbg}"
        );
    }

    #[test]
    fn spill_file_dropped_while_reader_open_keeps_spilled_bytes_until_reader_drops() {
        let dir = tempfile::tempdir().unwrap();
        let spill = SpillManager::with_fixed_cap(dir.path().to_path_buf(), 1024 * 1024);
        let b1 = sample_batch(16);
        let b2 = sample_batch(8);

        let sf = {
            let mut w = spill.create_file().unwrap();
            w.write_batch(&b1).unwrap();
            w.write_batch(&b2).unwrap();
            w.finish().unwrap()
        };
        let expected_len = sf.byte_len();
        assert!(expected_len > 0);
        assert_eq!(spill.spilled_bytes(), expected_len);

        let mut reader = sf.open_reader(Arc::clone(b1.schema())).unwrap();
        // Drop SpillFile while SpillReader is still open!
        drop(sf);
        assert_eq!(spill.spilled_bytes(), expected_len);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

        let got1 = reader.next_batch(None).unwrap().unwrap();
        assert!(got1.semantic_eq(&b1));
        let got2 = reader.next_batch(None).unwrap().unwrap();
        assert!(got2.semantic_eq(&b2));
        assert!(reader.next_batch(None).unwrap().is_none());

        // Dropping SpillReader closes FD and unlinks + subtracts bytes.
        drop(reader);
        assert_eq!(spill.spilled_bytes(), 0);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn spill_appender_lazy_creation_poison_on_error_and_zero_idle_fds() {
        let dir = tempfile::tempdir().unwrap();
        let spill = SpillManager::with_fixed_cap(dir.path().to_path_buf(), 1024 * 1024);
        let b1 = sample_batch(16);
        let b2 = sample_batch(12);

        // Unused appender creates 0 files and returns None
        let unused = spill.create_appender();
        assert_eq!(unused.byte_len(), 0);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(unused.finish().is_none());
        assert_eq!(spill.spilled_bytes(), 0);

        // Creating 100 appenders opens 0 FDs and creates 0 files
        let mut appenders: Vec<_> = (0..100).map(|_| spill.create_appender()).collect();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);

        // Appending to one creates only 1 file and holds 0 idle FDs into `dir` between calls
        let count_dir_fds = || -> Option<usize> {
            let entries = fs::read_dir("/proc/self/fd").ok()?;
            Some(
                entries
                    .filter_map(Result::ok)
                    .filter_map(|e| fs::read_link(e.path()).ok())
                    .filter(|target| target.starts_with(dir.path()))
                    .count(),
            )
        };
        if let Some(n) = count_dir_fds() {
            assert_eq!(n, 0);
        }
        appenders[0].append_batch(&b1, None).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        if let Some(n) = count_dir_fds() {
            assert_eq!(n, 0, "appender held an idle FD after first append");
        }
        appenders[0].append_batch(&b2, None).unwrap();
        if let Some(n) = count_dir_fds() {
            assert_eq!(n, 0, "appender held an idle FD after second append");
        }
        assert_eq!(spill.spilled_bytes(), appenders[0].byte_len());

        let app0 = appenders.remove(0);
        for unused_app in appenders {
            assert!(unused_app.finish().is_none());
        }
        let sf = app0
            .finish()
            .expect("used appender should produce SpillFile");
        assert_eq!(spill.spilled_bytes(), sf.byte_len());
        let mut reader = sf.open_reader(Arc::clone(b1.schema())).unwrap();
        assert!(reader.next_batch(None).unwrap().unwrap().semantic_eq(&b1));
        assert!(reader.next_batch(None).unwrap().unwrap().semantic_eq(&b2));
        assert!(reader.next_batch(None).unwrap().is_none());
        drop(reader);
        drop(sf);
        assert_eq!(spill.spilled_bytes(), 0);

        // Poisoned appender rejects subsequent appends and cleans up on finish/drop
        let mut poisoned_app = spill.create_appender();
        poisoned_app.append_batch(&b1, None).unwrap();
        poisoned_app.poisoned = true;
        let err = poisoned_app.append_batch(&b2, None).unwrap_err();
        assert!(matches!(err, QueryError::Internal(_)));
        assert!(poisoned_app.finish().is_none());
        assert_eq!(spill.spilled_bytes(), 0);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);

        // Uncommitted appender drop cleans up file and resets spilled_bytes
        {
            let mut uncommitted = spill.create_appender();
            uncommitted.append_batch(&b1, None).unwrap();
            assert!(spill.spilled_bytes() > 0);
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        }
        assert_eq!(spill.spilled_bytes(), 0);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
