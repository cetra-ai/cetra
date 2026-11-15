//! One open WAL segment.
//!
//! The writer task rotates between groups. [`SegmentWriter::needs_rotation`]
//! is therefore checked after a group is durable, never in the middle of one,
//! and a single group may leave the file larger than `segment_bytes`.
//!
//! Called from tests until the writer task lands.
#![allow(dead_code)]

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::error::{HeaderProblem, WalError};
use crate::format::FORMAT_VERSION;
use crate::fs::{self, File, OpenOptions};
use crate::hooks::{HookPoint, Hooks};
use crate::wal::segment_name;

/// First bytes of every segment.
pub(crate) const SEGMENT_MAGIC: [u8; 4] = *b"LWAL";
/// Length of the segment header.
pub(crate) const SEGMENT_HEADER_LEN: usize = 8;

/// The header this build writes: the magic, the format version as a `u16`
/// little-endian, and flags, which are 0.
pub(crate) fn segment_header() -> [u8; SEGMENT_HEADER_LEN] {
    let mut header = [0; SEGMENT_HEADER_LEN];
    header[..4].copy_from_slice(&SEGMENT_MAGIC);
    header[4..6].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    header
}

/// The header followed by `bytes`: the file a segment holds after `bytes` are written.
#[cfg(test)]
pub(crate) fn with_header(bytes: &[u8]) -> Vec<u8> {
    let mut out = segment_header().to_vec();
    out.extend_from_slice(bytes);
    out
}

/// What a segment's first bytes say.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HeaderState {
    /// A complete, supported header.
    Valid,
    /// Fewer than `SEGMENT_HEADER_LEN` bytes, all equal to the start of the header:
    /// a crash created the file before its header was complete.
    Torn,
}

/// Checks `bytes` (the whole file, or its first bytes) against the header.
///
/// Errors: `WalError::Header` with `NotAWalSegment` (the magic differs, or the file
/// is shorter than the header and is not a prefix of it), `NewerFormat { found, supported }`
/// (the version is not `FORMAT_VERSION`; no older version exists, so the name says newer and the message does not) or `UnknownFlags`.
pub(crate) fn check_header(
    table: &'static str,
    segment: &str,
    bytes: &[u8],
) -> Result<HeaderState, WalError> {
    let fail = |problem| WalError::Header {
        table,
        segment: segment.to_owned(),
        problem,
    };
    if bytes.len() < SEGMENT_HEADER_LEN {
        return if segment_header().starts_with(bytes) {
            Ok(HeaderState::Torn)
        } else {
            Err(fail(HeaderProblem::NotAWalSegment))
        };
    }
    if bytes[..4] != SEGMENT_MAGIC {
        return Err(fail(HeaderProblem::NotAWalSegment));
    }
    let found = u16::from_le_bytes([bytes[4], bytes[5]]);
    if found != FORMAT_VERSION {
        return Err(fail(HeaderProblem::NewerFormat {
            found,
            supported: FORMAT_VERSION,
        }));
    }
    if bytes[6..8] != [0, 0] {
        return Err(fail(HeaderProblem::UnknownFlags));
    }
    Ok(HeaderState::Valid)
}

/// An append-only segment file.
#[derive(Debug)]
pub(crate) struct SegmentWriter {
    dir: PathBuf,
    file: File,
    name: String,
    len: u64,
    segment_bytes: u64,
    hooks: Hooks,
}

impl SegmentWriter {
    /// Creates `dir/<first_seq>.wal` with its header, fsyncs the file, then fsyncs `dir`.
    ///
    /// A file of that name that holds no record is reused: an empty one, or one
    /// whose header was torn by a crash, is rewritten with the header; a complete
    /// header is left as it is. A file with records is left alone and this
    /// returns an error.
    pub(crate) fn create(
        dir: &Path,
        first_seq: u64,
        segment_bytes: u64,
        hooks: Hooks,
    ) -> Result<Self, WalError> {
        let name = segment_name(first_seq);
        let path = dir.join(&name);
        let file = open_segment(&path)?;
        // From here a failure leaves the file behind, possibly without its
        // header. The rotation path removes it; see `writer::remove_stray_segment`.
        if let Some(source) = hooks.fail(HookPoint::WalSegmentDirSync) {
            return Err(WalError::io("syncing WAL directory", dir, source));
        }
        fs::sync_dir(dir).map_err(|source| WalError::io("syncing WAL directory", dir, source))?;
        let len = SEGMENT_HEADER_LEN as u64;
        tracing::info!(segment = name.as_str(), bytes = len, "created WAL segment");
        Ok(Self {
            dir: dir.to_path_buf(),
            file,
            name,
            len,
            segment_bytes,
            hooks,
        })
    }

    /// Segment file name, for example `00000000000000000042.wal`.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Bytes in the file: the header, then the records written through this writer.
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// Whether the segment holds a record, as opposed to only its header.
    pub(crate) fn has_records(&self) -> bool {
        self.len > SEGMENT_HEADER_LEN as u64
    }

    /// Absolute path of the segment file.
    pub(crate) fn path(&self) -> PathBuf {
        self.dir.join(&self.name)
    }

    /// Appends `bytes` and advances [`Self::len`].
    ///
    /// The `WalWrite` hook runs first. An injected fault writes nothing.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<(), WalError> {
        self.hooks.pause(HookPoint::WalWrite);
        if let Some(source) = self.hooks.fail(HookPoint::WalWrite) {
            return Err(self.io("writing WAL segment", source));
        }
        self.file
            .write_all(bytes)
            .map_err(|source| self.io("writing WAL segment", source))?;
        self.len += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        Ok(())
    }

    /// `fdatasync` on the segment file. The `WalSync` hook runs first.
    pub(crate) fn sync(&mut self) -> Result<(), WalError> {
        self.hooks.pause(HookPoint::WalSync);
        if let Some(source) = self.hooks.fail(HookPoint::WalSync) {
            return Err(self.io("syncing WAL segment", source));
        }
        self.file
            .sync_data()
            .map_err(|source| self.io("syncing WAL segment", source))?;
        Ok(())
    }

    /// Drops any bytes past `len` and fsyncs the file.
    ///
    /// A group that was written but not acknowledged is removed, so the next
    /// replay does not treat it as a committed record.
    pub(crate) fn rewind(&mut self, len: u64) -> Result<(), WalError> {
        self.file
            .set_len(len)
            .map_err(|source| self.io("truncating WAL segment", source))?;
        self.file
            .sync_all()
            .map_err(|source| self.io("syncing WAL segment", source))?;
        self.len = len;
        Ok(())
    }

    /// The file is at least `segment_bytes`.
    ///
    /// Rotation happens after the group that crossed the limit, so the file
    /// can be larger than the limit by that one group.
    pub(crate) fn needs_rotation(&self) -> bool {
        self.len >= self.segment_bytes
    }

    fn io(&self, action: &'static str, source: io::Error) -> WalError {
        WalError::io(action, &self.dir.join(&self.name), source)
    }
}

/// Opens `path` for appending with a complete header in place, and syncs the
/// file when it writes the header.
///
/// A new file gets the header. An existing file is reused only when it holds no
/// record: a complete header is left untouched (opening writes nothing), and a
/// file shorter than the header that is a prefix of it is truncated and
/// rewritten. Anything else is an error.
fn open_segment(path: &Path) -> Result<File, WalError> {
    let mut options = OpenOptions::new();
    options.append(true);
    match options.create_new(true).open(path) {
        Ok(file) => write_header(file, path),
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::metadata(path)
                .map_err(|source| WalError::io("creating WAL segment", path, source))?;
            if meta.len() > SEGMENT_HEADER_LEN as u64 {
                return Err(WalError::io("creating WAL segment", path, source));
            }
            let bytes = fs::read(path)
                .map_err(|source| WalError::io("creating WAL segment", path, source))?;
            if !segment_header().starts_with(&bytes) {
                return Err(WalError::io("creating WAL segment", path, source));
            }
            let file = OpenOptions::new()
                .append(true)
                .open(path)
                .map_err(|source| WalError::io("creating WAL segment", path, source))?;
            if bytes.len() == SEGMENT_HEADER_LEN {
                return Ok(file);
            }
            file.set_len(0)
                .map_err(|source| WalError::io("creating WAL segment", path, source))?;
            write_header(file, path)
        }
        Err(source) => Err(WalError::io("creating WAL segment", path, source)),
    }
}

/// Writes the header at the end of the empty `file` and syncs it. The header
/// is synced at creation, before any record, so a power loss can cut a segment
/// only inside its records (spec 03 § 1). The `WalWrite` and `WalSync` hooks
/// are not consulted: they model record writes.
fn write_header(mut file: File, path: &Path) -> Result<File, WalError> {
    file.write_all(&segment_header())
        .map_err(|source| WalError::io("writing WAL segment", path, source))?;
    file.sync_data()
        .map_err(|source| WalError::io("syncing WAL segment", path, source))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::hooks::HookPoint;
    use crate::wal::record::encode;
    use crate::wal::{parse_segment_name, sample_span_batch, segment_name};

    fn assert_io(err: WalError, action: &str) {
        match err {
            WalError::Io {
                action: got,
                source,
                ..
            } => {
                assert_eq!(got, action);
                assert_eq!(source.to_string(), "injected fault");
            }
            other => panic!("expected an I/O error, got {other}"),
        }
    }

    #[test]
    fn segment_names_round_trip_and_reject_the_rest() {
        for seq in [0, 42, u64::MAX] {
            assert_eq!(parse_segment_name(&segment_name(seq)), Some(seq));
        }
        assert!(parse_segment_name("42.wal").is_none());
        assert!(parse_segment_name("000000000000000000042.wal").is_none());
        assert!(parse_segment_name("0000000000000000004g.wal").is_none());
        assert!(parse_segment_name("00000000000000000042.WAL").is_none());
        assert!(parse_segment_name("00000000000000000042.wal.bak").is_none());
        assert!(parse_segment_name("99999999999999999999.wal").is_none());
        assert!(parse_segment_name("").is_none());
    }

    #[test]
    fn a_new_segment_starts_with_the_header_and_counts_it() {
        let dir = tempfile::tempdir().unwrap();
        let writer = SegmentWriter::create(dir.path(), 1, 1024, Hooks::default()).unwrap();
        assert_eq!(writer.len(), SEGMENT_HEADER_LEN as u64);
        assert!(!writer.has_records());
        let bytes = fs::read(dir.path().join(writer.name())).unwrap();
        assert_eq!(bytes, [0x4c, 0x57, 0x41, 0x4c, 0x01, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn a_group_may_pass_the_segment_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = SegmentWriter::create(dir.path(), 1, 64, Hooks::default()).unwrap();
        assert!(!writer.needs_rotation());
        writer.write(&[0; 80]).unwrap();
        assert!(writer.needs_rotation());
        assert!(writer.has_records());
        assert_eq!(writer.len(), SEGMENT_HEADER_LEN as u64 + 80);
        assert_eq!(
            fs::metadata(dir.path().join(writer.name())).unwrap().len(),
            SEGMENT_HEADER_LEN as u64 + 80
        );
    }

    #[test]
    fn an_empty_segment_left_behind_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(segment_name(7));
        fs::write(&path, b"").unwrap();
        let mut writer = SegmentWriter::create(dir.path(), 7, 1024, Hooks::default()).unwrap();
        writer.write(b"abc").unwrap();
        assert_eq!(fs::read(&path).unwrap(), with_header(b"abc"));
    }

    #[test]
    fn a_header_only_segment_is_reused_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(segment_name(7));
        fs::write(&path, segment_header()).unwrap();
        let writer = SegmentWriter::create(dir.path(), 7, 1024, Hooks::default()).unwrap();
        assert!(!writer.has_records());
        assert_eq!(fs::read(&path).unwrap(), segment_header());
    }

    #[test]
    fn a_torn_header_is_rewritten_in_full() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(segment_name(7));
        for cut in 1..SEGMENT_HEADER_LEN {
            fs::write(&path, &segment_header()[..cut]).unwrap();
            let writer = SegmentWriter::create(dir.path(), 7, 1024, Hooks::default()).unwrap();
            assert_eq!(writer.len(), SEGMENT_HEADER_LEN as u64, "cut {cut}");
            assert_eq!(fs::read(&path).unwrap(), segment_header(), "cut {cut}");
        }
    }

    #[test]
    fn a_torn_header_lookalike_is_not_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(segment_name(7));
        fs::write(&path, b"LWAX").unwrap();
        let err = SegmentWriter::create(dir.path(), 7, 1024, Hooks::default()).unwrap_err();
        assert!(matches!(
            err,
            WalError::Io {
                action: "creating WAL segment",
                ..
            }
        ));
        assert_eq!(fs::read(&path).unwrap(), b"LWAX");
    }

    /// Without the header's `sync_data` this file is empty after the crash:
    /// its directory entry is synced, its unsynced bytes are not.
    #[test]
    fn a_created_segment_keeps_its_header_through_a_lossy_crash() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        fs::create_dir(&data).unwrap();
        let lossy = crate::fs::lossy::Lossy::enable(&data, &dir.path().join("shadow"));
        let writer = SegmentWriter::create(&data, 1, 1024, Hooks::default()).unwrap();
        let path = writer.path();
        drop(writer);
        lossy.crash_with(crate::fs::lossy::CrashMode::DropAll, 0);
        assert_eq!(
            fs::read(&path).unwrap(),
            segment_header(),
            "the header was lost with the crash"
        );
    }

    #[test]
    fn a_non_empty_segment_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(segment_name(7));
        fs::write(&path, b"already").unwrap();
        let err = SegmentWriter::create(dir.path(), 7, 1024, Hooks::default()).unwrap_err();
        match err {
            WalError::Io { action, source, .. } => {
                assert_eq!(action, "creating WAL segment");
                assert_eq!(source.kind(), io::ErrorKind::AlreadyExists);
            }
            other => panic!("expected AlreadyExists, got {other}"),
        }
        assert_eq!(fs::read(&path).unwrap(), b"already");
    }

    #[test]
    fn injected_write_and_sync_faults_surface() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks::default();
        hooks.arm_fail(HookPoint::WalWrite);
        // Creation is not hooked: the armed fault hits the first record.
        let mut writer = SegmentWriter::create(dir.path(), 1, 1024, hooks.clone()).unwrap();
        assert_io(writer.write(b"nope").unwrap_err(), "writing WAL segment");
        assert_eq!(writer.len(), SEGMENT_HEADER_LEN as u64);
        assert_eq!(
            fs::read(dir.path().join(writer.name())).unwrap(),
            segment_header()
        );

        writer.write(b"kept").unwrap();
        hooks.arm_fail(HookPoint::WalSync);
        assert_io(writer.sync().unwrap_err(), "syncing WAL segment");
        assert_eq!(
            fs::read(dir.path().join(writer.name())).unwrap(),
            with_header(b"kept")
        );
        writer.sync().unwrap();
    }

    #[test]
    fn records_land_in_a_new_segment_after_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let batch = sample_span_batch();
        let hooks = Hooks::default();
        let mut writer = SegmentWriter::create(dir.path(), 1, 1, hooks.clone()).unwrap();
        for seq in 1..=3 {
            if writer.needs_rotation() {
                writer = SegmentWriter::create(dir.path(), seq, 1, hooks.clone()).unwrap();
            }
            let mut buf = Vec::new();
            encode(seq, &batch, &mut buf).unwrap();
            writer.write(&buf).unwrap();
            writer.sync().unwrap();
        }
        for seq in 1..=3 {
            assert!(dir.path().join(segment_name(seq)).is_file());
        }
    }
}
