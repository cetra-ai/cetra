//! Replay a table's WAL directory.
//!
//! Spec 03 §1 recovery. A short read or a bad checksum at the tail of the last
//! segment is a torn write and is truncated. The same failure in an earlier
//! segment, a sequence that does not increase, or a checksum-valid payload
//! that is not one batch of this table, is corruption.
//!
//! Called from tests until `Storage::open` lands.
use std::path::Path;

use cetra_column::Batch;

use crate::error::WalError;
use crate::fs;
use crate::wal::record::{Decoded, decode_next, read_batch};
use crate::wal::segment::{HeaderState, SEGMENT_HEADER_LEN, check_header};
use crate::wal::{Table, segment_name};

/// What [`replay`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplayOutcome {
    /// `max(last valid seq, after_seq) + 1`. An empty directory is `after_seq + 1`.
    pub next_seq: u64,
    /// Records passed to `on_batch`. Records at or below `after_seq` are not counted.
    pub records: u64,
    /// Bytes removed from a torn tail.
    pub truncated_bytes: u64,
    /// Bytes left unread after a corrupt record when `skip_corrupt` is set.
    pub skipped_bytes: u64,
    /// The last segment in seq order, if the directory has one.
    pub last_segment: Option<std::path::PathBuf>,
}

/// Reads every segment in `dir`, in seq order, and calls `on_batch` for each
/// record newer than `after_seq`.
///
/// `on_batch` is not called for skipped seqs, and those records are not decoded.
/// `read_batch` failing is corruption wherever the record sits: the
/// checksum matched, so the bytes are what was written. With `skip_corrupt`,
/// that one record is skipped. A torn tail is truncated even when `skip_corrupt`
/// is set.
pub(crate) fn replay(
    dir: &Path,
    table: Table,
    after_seq: u64,
    skip_corrupt: bool,
    on_batch: &mut dyn FnMut(u64, Batch) -> Result<(), WalError>,
) -> Result<ReplayOutcome, WalError> {
    let segments = list_segments(dir)?;
    let mut progress = Progress {
        previous: None,
        last_valid: None,
        records: 0,
        truncated_bytes: 0,
        skipped_bytes: 0,
    };

    for (index, (first_seq, path)) in segments.iter().enumerate() {
        let name = segment_name(*first_seq);
        let bytes =
            fs::read(path).map_err(|source| WalError::io("reading WAL segment", path, source))?;
        let is_last = index + 1 == segments.len();
        let mut scan = Scan {
            table,
            name: &name,
            after_seq,
            skip_corrupt,
            is_last,
            progress: &mut progress,
        };
        if !scan_segment(&bytes, path, &mut scan, on_batch)? {
            break;
        }
    }

    let base = progress
        .last_valid
        .map_or(after_seq, |seq| seq.max(after_seq));
    let Some(next_seq) = base.checked_add(1) else {
        let segment = segments
            .last()
            .map(|(seq, _)| segment_name(*seq))
            .unwrap_or_default();
        return Err(WalError::Corrupt {
            table: table.name(),
            segment,
            offset: 0,
        });
    };
    let outcome = ReplayOutcome {
        next_seq,
        records: progress.records,
        truncated_bytes: progress.truncated_bytes,
        skipped_bytes: progress.skipped_bytes,
        last_segment: segments.last().map(|(_, path)| path.clone()),
    };
    tracing::info!(
        table = table.name(),
        records = outcome.records,
        next_seq = outcome.next_seq,
        truncated_bytes = outcome.truncated_bytes,
        skipped_bytes = outcome.skipped_bytes,
        "replayed WAL"
    );
    Ok(outcome)
}

struct Progress {
    previous: Option<u64>,
    last_valid: Option<u64>,
    records: u64,
    truncated_bytes: u64,
    skipped_bytes: u64,
}

/// One pass over a segment's bytes.
struct Scan<'a> {
    table: Table,
    name: &'a str,
    after_seq: u64,
    skip_corrupt: bool,
    is_last: bool,
    progress: &'a mut Progress,
}

/// Returns `true` when the caller should read the next segment.
fn scan_segment(
    bytes: &[u8],
    path: &Path,
    scan: &mut Scan<'_>,
    on_batch: &mut dyn FnMut(u64, Batch) -> Result<(), WalError>,
) -> Result<bool, WalError> {
    let mut offset = match check_header(scan.table.name(), scan.name, bytes)? {
        HeaderState::Valid => SEGMENT_HEADER_LEN,
        HeaderState::Torn => {
            // A header prefix holds no record: a crash cut the creation short.
            // The last segment is rewritten by the `create` that follows. An
            // earlier one can be skipped; it is not corruption, and replaying
            // again must not see it differently.
            if scan.is_last {
                truncate_tail(path, 0, bytes.len(), scan)?;
                return Ok(false);
            }
            tracing::warn!(
                table = scan.table.name(),
                segment = scan.name,
                bytes = bytes.len(),
                "skipping WAL segment with a torn header"
            );
            return Ok(true);
        }
    };
    while offset < bytes.len() {
        match decode_next(bytes, offset) {
            Decoded::End => break,
            Decoded::Record { seq, payload, next } => {
                if scan.progress.previous.is_some_and(|prev| seq <= prev) {
                    skip_rest(scan, offset, bytes.len() - offset)?;
                    break;
                }
                scan.progress.previous = Some(seq);
                if seq > scan.after_seq {
                    match read_batch(&bytes[payload], scan.table) {
                        Ok(batch) => {
                            on_batch(seq, batch)?;
                            scan.progress.records += 1;
                            scan.progress.last_valid = Some(seq);
                        }
                        Err(_) => {
                            if !scan.skip_corrupt {
                                return Err(corrupt(scan, offset));
                            }
                            tracing::error!(
                                table = scan.table.name(),
                                segment = scan.name,
                                offset = as_u64(offset),
                                "skipping corrupt WAL record"
                            );
                            scan.progress.skipped_bytes += as_u64(next - offset);
                        }
                    }
                } else {
                    scan.progress.last_valid = Some(seq);
                }
                offset = next;
            }
            Decoded::Short { offset: at } | Decoded::Invalid { offset: at, .. } => {
                if scan.is_last {
                    truncate_tail(path, at, bytes.len() - at, scan)?;
                    return Ok(false);
                }
                skip_rest(scan, at, bytes.len() - at)?;
                break;
            }
        }
    }
    Ok(true)
}

fn truncate_tail(
    path: &Path,
    at: usize,
    dropped: usize,
    scan: &mut Scan<'_>,
) -> Result<(), WalError> {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|source| WalError::io("truncating WAL segment", path, source))?;
    let len = as_u64(at);
    file.set_len(len)
        .map_err(|source| WalError::io("truncating WAL segment", path, source))?;
    file.sync_all()
        .map_err(|source| WalError::io("syncing WAL segment", path, source))?;
    let dropped = as_u64(dropped);
    scan.progress.truncated_bytes += dropped;
    tracing::warn!(
        table = scan.table.name(),
        segment = scan.name,
        offset = len,
        bytes = dropped,
        "truncated torn WAL tail"
    );
    Ok(())
}

fn skip_rest(scan: &mut Scan<'_>, offset: usize, rest: usize) -> Result<(), WalError> {
    if !scan.skip_corrupt {
        return Err(corrupt(scan, offset));
    }
    let bytes = as_u64(rest);
    tracing::error!(
        table = scan.table.name(),
        segment = scan.name,
        offset = as_u64(offset),
        bytes,
        "skipping corrupt WAL data"
    );
    scan.progress.skipped_bytes += bytes;
    Ok(())
}

fn corrupt(scan: &Scan<'_>, offset: usize) -> WalError {
    WalError::Corrupt {
        table: scan.table.name(),
        segment: scan.name.to_owned(),
        offset: as_u64(offset),
    }
}

fn as_u64(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// Checks the header of every segment of `table` in `dir` without changing
/// anything (spec 03 § 12). `open` calls it for both tables before it writes
/// to the directory, so a segment this build does not read refuses the open
/// with no file changed. A torn header passes: replay handles it. A missing
/// directory has no segments.
pub(crate) fn check_headers(dir: &Path, table: Table) -> Result<(), WalError> {
    use std::io::Read;

    match fs::metadata(dir) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(WalError::io("reading WAL directory", dir, source)),
        Ok(_) => {}
    }
    for (first_seq, path) in list_segments(dir)? {
        let mut file = fs::File::open(&path)
            .map_err(|source| WalError::io("reading WAL segment", &path, source))?;
        let mut header = Vec::with_capacity(SEGMENT_HEADER_LEN);
        file.by_ref()
            .take(SEGMENT_HEADER_LEN as u64)
            .read_to_end(&mut header)
            .map_err(|source| WalError::io("reading WAL segment", &path, source))?;
        check_header(table.name(), &segment_name(first_seq), &header)?;
    }
    Ok(())
}

fn list_segments(dir: &Path) -> Result<Vec<(u64, std::path::PathBuf)>, WalError> {
    let entries =
        fs::read_dir(dir).map_err(|source| WalError::io("reading WAL directory", dir, source))?;
    let mut segments = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| WalError::io("reading WAL directory", dir, source))?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        match crate::wal::parse_segment_name(&name) {
            Some(seq) => segments.push((seq, entry.path())),
            None => {
                tracing::warn!(file = name.as_ref(), "ignoring unrecognized WAL file");
            }
        }
    }
    segments.sort_by_key(|(seq, _)| *seq);
    Ok(segments)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::error::HeaderProblem;
    use crate::hooks::Hooks;
    use crate::wal::record::{encode, push_framed};
    use crate::wal::segment::{SegmentWriter, segment_header, with_header};
    use crate::wal::{sample_span_batch, segment_name};

    fn write_segments(dir: &Path, segment_bytes: u64, seqs: &[u64]) {
        let batch = sample_span_batch();
        let hooks = Hooks::default();
        let mut writer = SegmentWriter::create(dir, seqs[0], segment_bytes, hooks.clone()).unwrap();
        for (index, seq) in seqs.iter().copied().enumerate() {
            if index > 0 && writer.needs_rotation() {
                writer = SegmentWriter::create(dir, seq, segment_bytes, hooks.clone()).unwrap();
            }
            let mut buf = Vec::new();
            encode(seq, &batch, &mut buf).unwrap();
            writer.write(&buf).unwrap();
            writer.sync().unwrap();
        }
    }

    fn write_bytes(dir: &Path, first_seq: u64, bytes: &[u8]) {
        let mut writer = SegmentWriter::create(dir, first_seq, u64::MAX, Hooks::default()).unwrap();
        writer.write(bytes).unwrap();
        writer.sync().unwrap();
    }

    fn collect(dir: &Path, after_seq: u64, skip_corrupt: bool) -> (ReplayOutcome, Vec<u64>) {
        let mut seqs = Vec::new();
        let outcome = replay(
            dir,
            Table::Spans,
            after_seq,
            skip_corrupt,
            &mut |seq, batch| {
                assert_eq!(batch.num_rows(), 1);
                seqs.push(seq);
                Ok(())
            },
        )
        .unwrap();
        (outcome, seqs)
    }

    fn encoded(seq: u64) -> Vec<u8> {
        let batch = sample_span_batch();
        let mut buf = Vec::new();
        encode(seq, &batch, &mut buf).unwrap();
        buf
    }

    #[test]
    fn an_empty_directory_starts_at_after_seq_plus_one() {
        let dir = tempfile::tempdir().unwrap();
        let (outcome, seqs) = collect(dir.path(), 0, false);
        assert_eq!(outcome.next_seq, 1);
        assert_eq!(outcome.records, 0);
        assert!(seqs.is_empty());
        let (outcome, _) = collect(dir.path(), 8, false);
        assert_eq!(outcome.next_seq, 9);
    }

    #[test]
    fn records_replay_across_segments_in_order() {
        let dir = tempfile::tempdir().unwrap();
        write_segments(dir.path(), 1, &[1, 2, 3]);
        for seq in 1..=3 {
            assert!(dir.path().join(segment_name(seq)).is_file());
        }
        let (outcome, seqs) = collect(dir.path(), 0, false);
        assert_eq!(seqs, [1, 2, 3]);
        assert_eq!(outcome.records, 3);
        assert_eq!(outcome.next_seq, 4);
        assert_eq!(outcome.truncated_bytes, 0);
        assert_eq!(outcome.skipped_bytes, 0);
    }

    #[test]
    fn after_seq_skips_records_without_reading_the_batch() {
        let dir = tempfile::tempdir().unwrap();
        let mut buf = Vec::new();
        push_framed(&mut buf, 1, b"not ipc");
        let good = encoded(2);
        buf.extend_from_slice(&good);
        write_bytes(dir.path(), 1, &buf);
        let (outcome, seqs) = collect(dir.path(), 1, false);
        assert_eq!(seqs, [2]);
        assert_eq!(outcome.records, 1);
        assert_eq!(outcome.next_seq, 3);
    }

    #[test]
    fn a_torn_tail_truncates_to_the_last_full_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(segment_name(1));
        let prefix = with_header(&encoded(1));
        let mut tail = Vec::new();
        push_framed(&mut tail, 2, &[0; 32]);
        for cut in 0..tail.len() {
            let mut file = prefix.clone();
            file.extend_from_slice(&tail[..cut]);
            fs::write(&path, &file).unwrap();
            let (first, seqs) = collect(dir.path(), 0, false);
            assert_eq!(seqs, [1]);
            if cut == 0 {
                assert_eq!(first.truncated_bytes, 0);
            } else {
                assert_eq!(first.truncated_bytes, cut as u64);
                assert_eq!(fs::metadata(&path).unwrap().len(), prefix.len() as u64);
            }
            let (second, seqs) = collect(dir.path(), 0, false);
            assert_eq!(seqs, [1]);
            assert_eq!(second.truncated_bytes, 0);
            assert_eq!(second.next_seq, 2);
        }
    }

    #[test]
    fn a_checksum_error_in_the_last_segment_is_a_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(segment_name(1));
        let prefix = with_header(&encoded(1));
        let mut tail = encoded(2);
        let flip_at = 20;
        tail[flip_at] ^= 0x01;
        let mut file = prefix.clone();
        file.extend_from_slice(&tail);
        fs::write(&path, &file).unwrap();
        let (first, seqs) = collect(dir.path(), 0, false);
        assert_eq!(seqs, [1]);
        assert_eq!(first.truncated_bytes, tail.len() as u64);
        assert_eq!(fs::metadata(&path).unwrap().len(), prefix.len() as u64);
        let (second, seqs) = collect(dir.path(), 0, false);
        assert_eq!(seqs, [1]);
        assert_eq!(second.truncated_bytes, 0);
        assert_eq!(second.records, 1);
    }

    #[test]
    fn a_real_record_cut_in_the_last_segment_is_a_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(segment_name(1));
        let prefix = with_header(&encoded(1));
        let tail = encoded(2);
        let cuts = [1, 16, tail.len() / 2, tail.len() - 1];
        for cut in cuts {
            let mut file = prefix.clone();
            file.extend_from_slice(&tail[..cut]);
            fs::write(&path, &file).unwrap();
            let (first, seqs) = collect(dir.path(), 0, false);
            assert_eq!(seqs, [1], "cut {cut}");
            assert_eq!(first.truncated_bytes, cut as u64, "cut {cut}");
            let (second, _) = collect(dir.path(), 0, false);
            assert_eq!(second.truncated_bytes, 0, "cut {cut}");
        }
    }

    #[test]
    fn corruption_in_a_middle_segment_stops_or_skips_the_rest_of_it() {
        let dir = tempfile::tempdir().unwrap();
        write_segments(dir.path(), 1, &[1, 2, 3]);
        let middle = dir.path().join(segment_name(2));
        let mut bytes = fs::read(&middle).unwrap();
        let flipped_len = bytes.len() as u64;
        bytes[30] ^= 0x01;
        fs::write(&middle, &bytes).unwrap();

        let err = replay(dir.path(), Table::Spans, 0, false, &mut |_, _| Ok(())).unwrap_err();
        match err {
            WalError::Corrupt {
                table,
                segment,
                offset,
            } => {
                assert_eq!(table, "spans");
                assert_eq!(segment, segment_name(2));
                assert_eq!(offset, SEGMENT_HEADER_LEN as u64);
            }
            other => panic!("expected corruption, got {other}"),
        }
        assert_eq!(fs::metadata(&middle).unwrap().len(), flipped_len);

        let (outcome, seqs) = collect(dir.path(), 0, true);
        assert_eq!(seqs, [1, 3]);
        assert_eq!(outcome.records, 2);
        assert_eq!(
            outcome.skipped_bytes,
            flipped_len - SEGMENT_HEADER_LEN as u64
        );
        assert_eq!(outcome.next_seq, 4);
        assert_eq!(outcome.truncated_bytes, 0);
    }

    #[test]
    fn a_seq_that_does_not_increase_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let first = encoded(4);
        let mut buf = first.clone();
        buf.extend_from_slice(&encoded(4));
        write_bytes(dir.path(), 4, &buf);
        let err = replay(dir.path(), Table::Spans, 0, false, &mut |_, _| Ok(())).unwrap_err();
        match err {
            WalError::Corrupt {
                offset, segment, ..
            } => {
                assert_eq!(segment, segment_name(4));
                assert_eq!(offset, (SEGMENT_HEADER_LEN + first.len()) as u64);
            }
            other => panic!("expected corruption, got {other}"),
        }

        let other = tempfile::tempdir().unwrap();
        write_bytes(other.path(), 5, &encoded(5));
        write_bytes(other.path(), 6, &encoded(5));
        let err = replay(other.path(), Table::Spans, 0, false, &mut |_, _| Ok(())).unwrap_err();
        match err {
            WalError::Corrupt {
                offset, segment, ..
            } => {
                assert_eq!(segment, segment_name(6));
                assert_eq!(offset, SEGMENT_HEADER_LEN as u64);
            }
            other => panic!("expected corruption, got {other}"),
        }
    }

    #[test]
    fn a_checksum_valid_payload_that_is_not_a_batch_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let mut bad = Vec::new();
        push_framed(&mut bad, 1, b"not ipc");
        let bad_len = bad.len() as u64;
        let mut buf = bad.clone();
        buf.extend_from_slice(&encoded(2));
        write_bytes(dir.path(), 1, &buf);
        let path = dir.path().join(segment_name(1));
        let before = fs::metadata(&path).unwrap().len();

        let err = replay(dir.path(), Table::Spans, 0, false, &mut |_, _| Ok(())).unwrap_err();
        match err {
            WalError::Corrupt { offset, .. } => assert_eq!(offset, SEGMENT_HEADER_LEN as u64),
            other => panic!("expected corruption, got {other}"),
        }
        assert_eq!(fs::metadata(&path).unwrap().len(), before);

        let (outcome, seqs) = collect(dir.path(), 0, true);
        assert_eq!(seqs, [2]);
        assert_eq!(outcome.skipped_bytes, bad_len);
        assert_eq!(outcome.next_seq, 3);
        assert_eq!(outcome.truncated_bytes, 0);
    }

    #[test]
    fn unknown_files_are_ignored_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes.txt");
        fs::write(&notes, b"leave me").unwrap();
        let partial = dir.path().join("00000000000000000001.wal.tmp");
        fs::write(&partial, b"tmp").unwrap();
        write_bytes(dir.path(), 3, &encoded(3));
        let (outcome, seqs) = collect(dir.path(), 0, false);
        assert_eq!(seqs, [3]);
        assert_eq!(outcome.next_seq, 4);
        assert_eq!(fs::read(&notes).unwrap(), b"leave me");
        assert_eq!(fs::read(&partial).unwrap(), b"tmp");
    }

    /// Writes `bytes` as the whole file of segment `first_seq`, without a header of its own.
    fn raw_segment(dir: &Path, first_seq: u64, bytes: &[u8]) {
        fs::write(dir.join(segment_name(first_seq)), bytes).unwrap();
    }

    #[test]
    fn replay_reads_records_after_the_header() {
        let dir = tempfile::tempdir().unwrap();
        write_bytes(dir.path(), 1, &encoded(1));
        let bytes = fs::read(dir.path().join(segment_name(1))).unwrap();
        assert_eq!(&bytes[..SEGMENT_HEADER_LEN], &segment_header());
        match decode_next(&bytes, SEGMENT_HEADER_LEN) {
            Decoded::Record { seq, .. } => assert_eq!(seq, 1),
            other => panic!("expected the first record at offset 8, got {other:?}"),
        }
        let (outcome, seqs) = collect(dir.path(), 0, false);
        assert_eq!(seqs, [1]);
        assert_eq!(outcome.truncated_bytes, 0);
    }

    #[test]
    fn a_segment_with_a_newer_version_is_refused_untouched() {
        let mut header = b"LWAL".to_vec();
        header.extend_from_slice(&2_u16.to_le_bytes());
        header.extend_from_slice(&0_u16.to_le_bytes());
        let mut newer = header.clone();
        newer.extend_from_slice(&encoded(1));
        for skip_corrupt in [false, true] {
            // As the last segment, and as an earlier one followed by a valid segment.
            let dir = tempfile::tempdir().unwrap();
            raw_segment(dir.path(), 1, &newer);
            let path = dir.path().join(segment_name(1));
            let err = replay(
                dir.path(),
                Table::Spans,
                0,
                skip_corrupt,
                &mut |_, _| Ok(()),
            )
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    WalError::Header {
                        problem: HeaderProblem::NewerFormat {
                            found: 2,
                            supported: 1
                        },
                        ..
                    }
                ),
                "{err}"
            );
            assert_eq!(
                fs::read(&path).unwrap(),
                newer,
                "skip_corrupt {skip_corrupt}"
            );

            let dir = tempfile::tempdir().unwrap();
            raw_segment(dir.path(), 1, &newer);
            write_bytes(dir.path(), 2, &encoded(2));
            let err = replay(
                dir.path(),
                Table::Spans,
                0,
                skip_corrupt,
                &mut |_, _| Ok(()),
            )
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    WalError::Header {
                        problem: HeaderProblem::NewerFormat { .. },
                        ..
                    }
                ),
                "{err}"
            );
            assert_eq!(
                fs::read(&path).unwrap(),
                newer,
                "skip_corrupt {skip_corrupt}"
            );
        }
    }

    #[test]
    fn a_segment_with_other_flags_or_magic_is_refused_untouched() {
        let mut flagged = segment_header().to_vec();
        flagged[6] = 1;
        flagged.extend_from_slice(&encoded(1));
        let mut records_only = Vec::new();
        push_framed(&mut records_only, 1, b"not ipc");
        for skip_corrupt in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            raw_segment(dir.path(), 1, &flagged);
            let path = dir.path().join(segment_name(1));
            let err = replay(
                dir.path(),
                Table::Spans,
                0,
                skip_corrupt,
                &mut |_, _| Ok(()),
            )
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    WalError::Header {
                        problem: HeaderProblem::UnknownFlags,
                        ..
                    }
                ),
                "{err}"
            );
            assert_eq!(fs::read(&path).unwrap(), flagged);

            let dir = tempfile::tempdir().unwrap();
            raw_segment(dir.path(), 1, &records_only);
            let path = dir.path().join(segment_name(1));
            let err = replay(
                dir.path(),
                Table::Spans,
                0,
                skip_corrupt,
                &mut |_, _| Ok(()),
            )
            .unwrap_err();
            assert!(
                matches!(
                    err,
                    WalError::Header {
                        problem: HeaderProblem::NotAWalSegment,
                        ..
                    }
                ),
                "{err}"
            );
            assert_eq!(fs::read(&path).unwrap(), records_only);
        }
    }

    #[test]
    fn a_torn_header_is_a_torn_creation() {
        for cut in 0..SEGMENT_HEADER_LEN {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(segment_name(1));
            raw_segment(dir.path(), 1, &segment_header()[..cut]);
            let (outcome, seqs) = collect(dir.path(), 0, false);
            assert!(seqs.is_empty(), "cut {cut}");
            assert_eq!(outcome.records, 0, "cut {cut}");
            assert_eq!(outcome.truncated_bytes, cut as u64, "cut {cut}");
            assert_eq!(fs::metadata(&path).unwrap().len(), 0, "cut {cut}");
            // The `create` after a replay reuses the emptied file and writes the header.
            let writer = SegmentWriter::create(dir.path(), 1, u64::MAX, Hooks::default()).unwrap();
            assert!(!writer.has_records(), "cut {cut}");
            assert_eq!(fs::read(&path).unwrap(), segment_header(), "cut {cut}");
        }
    }

    #[test]
    fn a_complete_header_without_records_replays_empty_and_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(segment_name(1));
        raw_segment(dir.path(), 1, &segment_header());
        let (outcome, seqs) = collect(dir.path(), 0, false);
        assert!(seqs.is_empty());
        assert_eq!(outcome.records, 0);
        assert_eq!(outcome.truncated_bytes, 0);
        assert_eq!(outcome.next_seq, 1);
        let writer = SegmentWriter::create(dir.path(), 1, u64::MAX, Hooks::default()).unwrap();
        assert!(!writer.has_records());
        assert_eq!(fs::read(&path).unwrap(), segment_header());
    }

    #[test]
    fn a_short_file_that_is_not_a_header_prefix_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(segment_name(1));
        raw_segment(dir.path(), 1, b"LWAX");
        let err = replay(dir.path(), Table::Spans, 0, true, &mut |_, _| Ok(())).unwrap_err();
        assert!(
            matches!(
                err,
                WalError::Header {
                    problem: HeaderProblem::NotAWalSegment,
                    ..
                }
            ),
            "{err}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"LWAX");
    }

    #[test]
    fn a_torn_header_in_an_earlier_segment_is_skipped_silently() {
        let dir = tempfile::tempdir().unwrap();
        raw_segment(dir.path(), 1, b"");
        raw_segment(dir.path(), 2, &segment_header()[..4]);
        write_bytes(dir.path(), 3, &encoded(3));
        for skip_corrupt in [false, true] {
            let (outcome, seqs) = collect(dir.path(), 0, skip_corrupt);
            assert_eq!(seqs, [3], "skip_corrupt {skip_corrupt}");
            assert_eq!(outcome.records, 1);
            assert_eq!(outcome.skipped_bytes, 0);
            assert_eq!(outcome.truncated_bytes, 0);
            assert_eq!(outcome.next_seq, 4);
            assert_eq!(
                fs::metadata(dir.path().join(segment_name(1)))
                    .unwrap()
                    .len(),
                0
            );
            assert_eq!(
                fs::read(dir.path().join(segment_name(2))).unwrap(),
                &segment_header()[..4]
            );
        }
    }

    #[test]
    fn a_damaged_earlier_segment_after_a_valid_header_is_still_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let mut bad = encoded(1);
        bad[20] ^= 0x01;
        write_bytes(dir.path(), 1, &bad);
        write_bytes(dir.path(), 2, &encoded(2));
        let err = replay(dir.path(), Table::Spans, 0, false, &mut |_, _| Ok(())).unwrap_err();
        assert!(
            matches!(
                err,
                WalError::Corrupt { offset, .. } if offset == SEGMENT_HEADER_LEN as u64
            ),
            "{err}"
        );
        let (outcome, seqs) = collect(dir.path(), 0, true);
        assert_eq!(seqs, [2]);
        assert_eq!(outcome.skipped_bytes, bad.len() as u64);
    }

    /// A torn last segment is truncated, the next segment is created after it,
    /// and a second replay sees the same state. A stray empty segment left by a
    /// failed rotation is skipped the same way.
    #[test]
    fn replay_is_idempotent_after_a_torn_last_segment() {
        let dir = tempfile::tempdir().unwrap();
        raw_segment(dir.path(), 5, &segment_header()[..5]);
        let (first, seqs) = collect(dir.path(), 9, false);
        assert!(seqs.is_empty());
        assert_eq!(first.truncated_bytes, 5);
        assert_eq!(first.next_seq, 10);
        drop(SegmentWriter::create(dir.path(), 10, u64::MAX, Hooks::default()).unwrap());
        let (second, seqs) = collect(dir.path(), 9, false);
        assert!(seqs.is_empty());
        assert_eq!(second.next_seq, first.next_seq);
        assert_eq!(second.truncated_bytes, 0);

        let stray = tempfile::tempdir().unwrap();
        raw_segment(stray.path(), 3, b"");
        write_bytes(stray.path(), 4, &encoded(4));
        let (outcome, seqs) = collect(stray.path(), 0, false);
        assert_eq!(seqs, [4]);
        assert_eq!(outcome.next_seq, 5);
    }
}
