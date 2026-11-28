//! Stable-Rust replay tests for `fuzz/corpus/statam_file/` and `fuzz/regressions/statam_file/`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cetra_statam::format::{BLOCK_FLAG_FSST_SUBCHUNKED, Codec, Encoding, PACK_FLAG_HAS_DICT};
use cetra_statam::fuzz_entry::fuzz_statam_file;
use cetra_statam::{ChunkPackReader, InMemoryPackResolver, ScanRequest, SliceReader, StatamReader};

fn sorted_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .map(|entry| entry.expect("read corpus entry").path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
}

#[test]
fn replay_statam_file_corpus_and_regressions() {
    let fuzz_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz");
    let corpus = sorted_files(&fuzz_root.join("corpus/statam_file"));
    assert!(
        !corpus.is_empty(),
        "fuzz/corpus/statam_file must contain seed files"
    );

    let regressions = sorted_files(&fuzz_root.join("regressions/statam_file"));
    for path in corpus.into_iter().chain(regressions) {
        let data = std::fs::read(&path).expect("read corpus/regression file");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fuzz_statam_file(&data);
        }));
        assert!(outcome.is_ok(), "replay panicked on {}", path.display());
    }
}

#[test]
fn valid_statam_and_paired_seeds_verify_and_scan() {
    let corpus_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/statam_file");

    for name in [
        "seed-01-l0-spans",
        "seed-02-l1-spans",
        "seed-03-l1-traces",
        "seed-04-l0-scores",
        "seed-05-all16-l0",
        "seed-06-all16-l1",
        "seed-07-empty",
        "seed-08-shredded-nested",
        "seed-19-simd-1030-l1",
        "seed-21-fsst-subchunked-l1",
    ] {
        let data = std::fs::read(corpus_dir.join(name)).expect(name);
        let reader = StatamReader::open(Arc::new(SliceReader::new(data)), None).expect(name);
        reader.verify_full().expect(name);
        reader.scan(&ScanRequest::default()).expect(name);
        if name == "seed-21-fsst-subchunked-l1" {
            let has_subchunked = reader.meta().block_dir.iter().any(|b| {
                b.encoding == Encoding::Fsst
                    && (b.block_flags & BLOCK_FLAG_FSST_SUBCHUNKED) != 0
                    && b.codec == Codec::None
                    && b.raw_len == b.stored_len
            });
            assert!(
                has_subchunked,
                "seed-21-fsst-subchunked-l1 must contain a BLOCK_FLAG_FSST_SUBCHUNKED block"
            );
            let has_subchunked_list = reader
                .meta()
                .block_dir
                .iter()
                .any(|b| b.encoding == Encoding::ListEncoded && b.codec == Codec::None);
            assert!(
                has_subchunked_list,
                "seed-21-fsst-subchunked-l1 ListEncoded block with sub-chunked FSST child must use outer Codec::None"
            );
            let taken = reader.take_rows(&[0, 256, 512, 1029], None, 1).expect(name);
            assert_eq!(taken.len(), 1);
            assert_eq!(taken[0].num_rows(), 4);
        }
    }

    for name in [
        "seed-09-stpk-single",
        "seed-10-stpk-multi-frame",
        "seed-20-stpk-zstd-dict",
    ] {
        let data = std::fs::read(corpus_dir.join(name)).expect(name);
        let pack = ChunkPackReader::open(Arc::new(SliceReader::new(data))).expect(name);
        pack.read_edge_table().expect(name);
        pack.read_hash_table().expect(name);
        if name == "seed-20-stpk-zstd-dict" {
            assert_ne!(
                pack.postscript().flags & PACK_FLAG_HAS_DICT,
                0,
                "seed-20-stpk-zstd-dict must carry PACK_FLAG_HAS_DICT"
            );
            let chunks = pack.read_chunks(&[0, 1, 100]).expect(name);
            assert_eq!(chunks.len(), 3);
        }
    }

    for name in [
        "seed-11-paired-l1-spans",
        "seed-12-paired-multiturn",
        "seed-22-binary-blobs-stpk",
    ] {
        let data = std::fs::read(corpus_dir.join(name)).expect(name);
        let len_a = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let len_b = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
        let statam_bytes = data[8..8 + len_a].to_vec();
        let pack_bytes = data[8 + len_a..8 + len_a + len_b].to_vec();

        let pack = Arc::new(
            ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes.clone()))).unwrap(),
        );
        let mut resolver = InMemoryPackResolver::new();
        resolver.insert(pack.pack_id(), Arc::clone(&pack));
        let statam = StatamReader::open(
            Arc::new(SliceReader::new(statam_bytes.clone())),
            Some(Arc::new(resolver)),
        )
        .expect(name);
        statam.verify_full().expect(name);
        let batches = statam.scan(&ScanRequest::default()).expect(name);
        assert!(!batches.is_empty(), "{name} produced empty scan");
    }
}
