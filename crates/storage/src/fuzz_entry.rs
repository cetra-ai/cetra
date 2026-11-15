//! Fuzz entry points (track R M3, docs/plans/r-hardening/03-m3-fuzzing.md). Each one takes arbitrary bytes and must not panic.
#![allow(clippy::expect_used, clippy::unwrap_used)] // harness setup, not input-dependent

use std::path::PathBuf;

use uuid::Uuid;

use crate::config::WalConfig;
use crate::hooks::Hooks;
use crate::memtable::Memtable;
use crate::wal::{Decoded, Table, decode_next, encode, push_framed, read_batch, segment_name};
use crate::{recover_table, replay_batch};

/// Reads a fuzz input's header. Missing bytes read as zero, so every input is valid.
struct Cursor<'a> {
    data: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    fn u8(&mut self) -> u8 {
        self.bytes(1).first().copied().unwrap_or(0)
    }

    fn u32(&mut self) -> u32 {
        let mut buf = [0; 4];
        let bytes = self.bytes(4);
        buf[..bytes.len()].copy_from_slice(bytes);
        u32::from_le_bytes(buf)
    }

    fn u64(&mut self) -> u64 {
        let mut buf = [0; 8];
        let bytes = self.bytes(8);
        buf[..bytes.len()].copy_from_slice(bytes);
        u64::from_le_bytes(buf)
    }

    /// At most `n` bytes, fewer when the input is short.
    fn bytes(&mut self, n: usize) -> &'a [u8] {
        let n = n.min(self.data.len());
        let (head, tail) = self.data.split_at(n);
        self.data = tail;
        head
    }
}

/// A scratch directory under the system temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("llmobs-fuzz-{}", Uuid::now_v7()));
        // Harness setup, not input-dependent.
        std::fs::create_dir_all(&path).expect("create the fuzz scratch directory");
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Frames arbitrary bytes as WAL records and decodes them; the IPC payload is read as a batch.
pub fn wal_record(data: &[u8]) {
    let mut offset = 0;
    while let Decoded::Record { next, .. } = decode_next(data, offset) {
        assert!(next > offset, "framing must advance");
        offset = next;
    }
    for table in [Table::Spans, Table::Scores] {
        let Ok(batch) = read_batch(data, table) else {
            continue;
        };
        let _ = replay_batch(&mut Memtable::new(table), 1, batch.clone());

        let mut out = Vec::new();
        assert!(
            encode(1, &batch, &mut out).is_ok(),
            "a decoded batch re-encodes"
        );
        let Decoded::Record { payload, .. } = decode_next(&out, 0) else {
            panic!("a re-encoded record frames");
        };
        let again = read_batch(&out[payload], table).expect("a re-encoded batch decodes");
        assert_eq!(again.num_rows(), batch.num_rows());
        assert_eq!(again.schema().fields(), batch.schema().fields());
    }
}

/// What the `wal_replay` input says to do, after its segments are on disk.
struct Staged {
    table: Table,
    dir: PathBuf,
    after_seq: u64,
    skip_corrupt: bool,
}

/// Writes the input's segments into `scratch`, as `wal_replay` describes them.
fn stage(data: &[u8], scratch: &Scratch) -> Staged {
    let mut input = Cursor::new(data);
    let flags = input.u8();
    let table = if flags & 1 == 0 {
        Table::Spans
    } else {
        Table::Scores
    };
    let skip_corrupt = flags & 2 != 0;
    let segments = 1 + usize::from(input.u8()) % 3;
    let after_seq = input.u64();

    let dir = scratch.0.join(table.name());
    std::fs::create_dir_all(&dir).expect("create the WAL directory");
    for _ in 0..segments {
        let first_seq = input.u64();
        let mode = input.u8();
        let bytes = if mode & 1 == 0 {
            let mut segment = Vec::new();
            let frames = usize::from(input.u8()) % 5;
            for _ in 0..frames {
                let seq = input.u64();
                let len = usize::try_from(input.u32())
                    .unwrap_or(usize::MAX)
                    .min(1 << 20);
                push_framed(&mut segment, seq, input.bytes(len));
            }
            let tear = usize::from(mode >> 1) % 32;
            segment.truncate(segment.len().saturating_sub(tear));
            segment
        } else {
            let len = usize::try_from(input.u32())
                .unwrap_or(usize::MAX)
                .min(1 << 20);
            input.bytes(len).to_vec()
        };
        std::fs::write(dir.join(segment_name(first_seq)), bytes)
            .expect("write a WAL segment into the scratch directory");
    }
    Staged {
        table,
        dir,
        after_seq,
        skip_corrupt,
    }
}

/// Writes the fuzzed segments into a scratch WAL directory and replays it twice.
///
/// The entry frames most segments itself with the real checksum; see the layout in
/// `03-m3-fuzzing.md` § R3.1.
pub fn wal_replay(data: &[u8]) {
    let scratch = Scratch::new();
    let staged = stage(data, &scratch);
    let segment_bytes = WalConfig::default().segment_bytes.0;
    let hooks = Hooks::default();
    let replay = |staged: &Staged| {
        recover_table(
            &staged.dir,
            staged.table,
            staged.after_seq,
            staged.skip_corrupt,
            segment_bytes,
            &hooks,
        )
    };
    let Ok(first) = replay(&staged) else {
        return;
    };
    let records = first.records;
    let memtable_bytes = first.table.memtable.bytes();
    // Dropping the first replay releases the new segment, which the second one reuses.
    drop(first);

    let second = replay(&staged).expect("a replay that truncated its tail replays again");
    assert_eq!(
        second.records, records,
        "a second replay sees the same records"
    );
    assert_eq!(
        second.table.memtable.bytes(),
        memtable_bytes,
        "a second replay rebuilds the same memtable"
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use arrow::array::RecordBatch;

    use super::*;
    use crate::test_support::{dataset_scores, score_rows};
    use crate::wal::{HEADER_LEN, sample_span_batch};

    /// Runs `entry` on every file of `fuzz/corpus/<target>/` and
    /// `fuzz/regressions/<target>/`. The corpus must not be empty.
    fn replay(target: &str, entry: fn(&[u8])) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz");
        let corpus = sorted_files(&root.join("corpus").join(target));
        assert!(!corpus.is_empty(), "no corpus for {target}");
        let regressions = sorted_files(&root.join("regressions").join(target));
        for path in corpus.into_iter().chain(regressions) {
            let data = std::fs::read(&path).expect("read a corpus file");
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| entry(&data)));
            if outcome.is_err() {
                panic!("{target}: {} panicked", path.display());
            }
        }
    }

    /// The files in `dir`, sorted by name. A missing directory has none.
    fn sorted_files(dir: &Path) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .map(|entry| entry.expect("read a corpus entry").path())
            .filter(|path| path.is_file())
            .collect();
        files.sort();
        files
    }

    /// The fuzz corpus directory for one target.
    fn corpus_dir(target: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fuzz/corpus")
            .join(target)
    }

    /// The IPC payload of one batch of `table`, without its record header.
    fn ipc_payload(table: Table) -> Vec<u8> {
        let batch: RecordBatch = match table {
            Table::Spans => sample_span_batch(),
            Table::Scores => score_rows(&dataset_scores()),
        };
        let mut out = Vec::new();
        encode(1, &batch, &mut out).expect("encode a test batch");
        out[HEADER_LEN..].to_vec()
    }

    /// The entry input for `wal_replay`: one framed segment per `(first_seq, record seqs, tear)`.
    fn seed_wal_replay(
        table: Table,
        skip_corrupt: bool,
        segments: &[(u64, &[u64], u8)],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let table_bit = u8::from(table == Table::Scores);
        out.push(table_bit | (u8::from(skip_corrupt) << 1));
        out.push(u8::try_from(segments.len() - 1).expect("at most three segments"));
        out.extend_from_slice(&0_u64.to_le_bytes());
        for (first_seq, seqs, tear) in segments {
            out.extend_from_slice(&first_seq.to_le_bytes());
            out.push(tear << 1);
            out.push(u8::try_from(seqs.len()).expect("at most four frames"));
            for seq in *seqs {
                let payload = ipc_payload(table);
                out.extend_from_slice(&seq.to_le_bytes());
                out.extend_from_slice(
                    &u32::try_from(payload.len())
                        .expect("small payload")
                        .to_le_bytes(),
                );
                out.extend_from_slice(&payload);
            }
        }
        out
    }

    /// Runs the `wal_replay` steps once by hand and returns the record count.
    fn first_replay_records(data: &[u8]) -> u64 {
        let scratch = Scratch::new();
        let staged = stage(data, &scratch);
        let first = recover_table(
            &staged.dir,
            staged.table,
            staged.after_seq,
            staged.skip_corrupt,
            WalConfig::default().segment_bytes.0,
            &Hooks::default(),
        )
        .expect("a seeded WAL replays");
        first.records
    }

    #[test]
    fn wal_record_corpus_replays() {
        replay("wal_record", wal_record);
    }

    #[test]
    fn wal_replay_corpus_replays() {
        replay("wal_replay", wal_replay);
    }

    #[test]
    fn wal_replay_replays_framed_records() {
        let data = seed_wal_replay(Table::Spans, false, &[(1, &[1, 2, 3], 0)]);
        assert_eq!(first_replay_records(&data), 3);
    }

    #[test]
    fn wal_replay_truncates_a_torn_frame() {
        let data = seed_wal_replay(Table::Spans, false, &[(1, &[1, 2, 3], 5)]);
        assert_eq!(first_replay_records(&data), 2);
        // The entry's second replay must agree with the first; the entry asserts it.
        wal_replay(&data);
    }

    /// Writes the seed files of this crate's targets (track R M3, `just fuzz-seed`).
    #[test]
    #[ignore]
    fn write_seed_corpus() {
        let write = |target: &str, name: &str, bytes: &[u8]| {
            let dir = corpus_dir(target);
            std::fs::create_dir_all(&dir).expect("create the corpus directory");
            std::fs::write(dir.join(format!("seed-{name}")), bytes).expect("write a seed");
        };
        write("wal_record", "spans", &ipc_payload(Table::Spans));
        write("wal_record", "scores", &ipc_payload(Table::Scores));
        write(
            "wal_replay",
            "one-segment",
            &seed_wal_replay(Table::Spans, false, &[(1, &[1, 2, 3], 0)]),
        );
        write(
            "wal_replay",
            "torn-tail",
            &seed_wal_replay(Table::Spans, false, &[(1, &[1, 2, 3], 5)]),
        );
        write(
            "wal_replay",
            "two-segments",
            &seed_wal_replay(Table::Scores, true, &[(1, &[1, 2], 0), (3, &[3], 0)]),
        );
    }
}
