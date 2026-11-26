//! Stable-Rust replay tests for `fuzz/corpus/statam_file/` and `fuzz/regressions/statam_file/`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use cetra_column::DataType;
use cetra_statam::codec::dict::FileDict;
use cetra_statam::codec::{ZonePred, ZoneValue};
use cetra_statam::format::{MAX_BATCH_DECODED_BYTES, decode_schema};
use cetra_statam::payload::chunker::chunk_payload;
use cetra_statam::{
    ChunkPackReader, CoalesceMode, GdprFileInput, IdLookup, InMemoryPackResolver, PackResolver,
    Predicate, RangeReader, Sbbf, ScanRequest, SliceReader, StatamFileMeta, StatamReader,
    SubstringQuery, TimeRange, decode_payload_refs, gdpr_rewrite_day, search_pack_chunks,
};

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

fn replay_bytes(data: &[u8]) {
    let _ = decode_schema(data);
    let _ = Sbbf::from_bytes(data);
    let _ = FileDict::decode(data, false);
    let _ = FileDict::decode(data, true);

    let row_count_hint = if data.len() >= 2 {
        u16::from_le_bytes([data[0], data[1]]) as usize
    } else {
        data.len()
    };
    let payload_slice = if data.len() >= 2 { &data[2..] } else { data };
    let _ = decode_payload_refs(payload_slice, row_count_hint, MAX_BATCH_DECODED_BYTES);
    let _ = decode_payload_refs(data, 4, MAX_BATCH_DECODED_BYTES);

    let mut chunks = Vec::new();
    chunk_payload(data, &mut chunks);
    assert_eq!(chunks.iter().map(|c| c.len()).sum::<usize>(), data.len());

    // Standalone .statam
    let reader_arc = Arc::new(SliceReader::new(Bytes::copy_from_slice(data)));
    let _ = StatamFileMeta::read_from(reader_arc.as_ref(), Some(64));
    if let Ok(reader) = StatamReader::open(reader_arc, None) {
        let _ = reader.verify_full();
        exercise_queries(&reader);
        let inputs = [GdprFileInput {
            file_id: "replay-standalone".to_owned(),
            reader: Arc::new(SliceReader::new(Bytes::copy_from_slice(data))),
            write_options: None,
        }];
        let _ = gdpr_rewrite_day(&inputs, &HashMap::new(), &|_, row| row.is_multiple_of(2));
    }

    // Standalone .stpk
    let pack_arc = Arc::new(SliceReader::new(Bytes::copy_from_slice(data)));
    if let Ok(pack) = ChunkPackReader::open(pack_arc) {
        let _ = pack.read_edge_table();
        let _ = pack.read_trigram_section();
        let _ = pack.read_hash_table();
        let n_frames = pack.toc().frames.len().min(4);
        for fid in 0..n_frames {
            let _ = pack.read_frame_raw(fid as u32);
            let _ = pack.read_frame_decompressed(fid as u32);
        }
        let n_chunks = pack.toc().chunks.len().min(32);
        let ids: Vec<u32> = (0..n_chunks as u32).collect();
        let _ = pack.read_chunks(&ids);
        let _ = search_pack_chunks(&pack, b"err", false);
        let _ = search_pack_chunks(&pack, b"ERR", true);
    }

    // Paired .statam + .stpk
    if data.len() >= 8 {
        let len_a = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let len_b = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
        let exact_fit = 8usize
            .checked_add(len_a)
            .and_then(|end_a| end_a.checked_add(len_b))
            .is_some_and(|total| total <= data.len());
        if exact_fit {
            replay_paired(&data[8..8 + len_a], &data[8 + len_a..8 + len_a + len_b]);
        } else if data.len() > 8 {
            let body = &data[8..];
            let split = len_a % body.len();
            replay_paired(&body[..split], &body[split..]);
        }
    }
}

fn replay_paired(statam_bytes: &[u8], pack_bytes: &[u8]) {
    let pack_reader: Arc<dyn RangeReader> =
        Arc::new(SliceReader::new(Bytes::copy_from_slice(pack_bytes)));
    let pack_res = ChunkPackReader::open(Arc::clone(&pack_reader));

    let mut resolver = InMemoryPackResolver::new();
    let mut packs_map: HashMap<[u8; 16], Arc<dyn RangeReader>> = HashMap::new();
    if let Ok(pack) = pack_res {
        let pid = pack.pack_id();
        resolver.insert(pid, Arc::new(pack));
        packs_map.insert(pid, pack_reader);
    }
    let resolver_arc: Arc<dyn PackResolver> = Arc::new(resolver);

    if let Ok(statam) = StatamReader::open(
        Arc::new(SliceReader::new(Bytes::copy_from_slice(statam_bytes))),
        Some(resolver_arc),
    ) {
        let _ = statam.verify_full();
        exercise_queries(&statam);
    }

    let inputs = [GdprFileInput {
        file_id: "replay-paired".to_owned(),
        reader: Arc::new(SliceReader::new(Bytes::copy_from_slice(statam_bytes))),
        write_options: None,
    }];
    if let Ok(report) = gdpr_rewrite_day(&inputs, &packs_map, &|_, row| row.is_multiple_of(2)) {
        let mut new_resolver = InMemoryPackResolver::new();
        for np in &report.new_packs {
            let _ = new_resolver.insert_bytes(np.new_pack_id, Bytes::copy_from_slice(&np.bytes));
        }
        let new_res_arc: Arc<dyn PackResolver> = Arc::new(new_resolver);
        for rf in &report.rewritten_files {
            if let Ok(reopened) = StatamReader::open(
                Arc::new(SliceReader::new(Bytes::copy_from_slice(&rf.bytes))),
                Some(Arc::clone(&new_res_arc)),
            ) {
                let _ = reopened.verify_full();
                let _ = reopened.scan(&ScanRequest::default());
            }
        }
    }
}

fn exercise_queries(reader: &StatamReader) {
    let _ = reader.scan(&ScanRequest::default());
    let _ = reader.scan(&ScanRequest {
        projection: Some(Vec::new()),
        ..Default::default()
    });
    let _ = reader.scan(&ScanRequest {
        limit: Some(5),
        sort_desc_hint: true,
        coalesce_mode: CoalesceMode::Scan,
        ..Default::default()
    });
    let _ = reader.scan(&ScanRequest {
        limit: Some(3),
        coalesce_mode: CoalesceMode::Point,
        time_range: TimeRange::new(i64::MIN / 2, i64::MAX / 2),
        ..Default::default()
    });

    let meta = reader.meta();
    for field in meta.schema().fields() {
        match field.data_type {
            DataType::Utf8 => {
                let _ = reader.scan(&ScanRequest {
                    substrings: vec![SubstringQuery::like(&field.name, b"err")],
                    ..Default::default()
                });
                let _ = reader.scan(&ScanRequest {
                    substrings: vec![SubstringQuery::ilike(&field.name, b"PROMPT")],
                    ..Default::default()
                });
            }
            DataType::FixedBytes16 => {
                let _ = reader.scan(&ScanRequest {
                    id_lookups: vec![IdLookup::eq(&field.name, [0u8; 16])],
                    coalesce_mode: CoalesceMode::Point,
                    ..Default::default()
                });
            }
            DataType::MapUtf8 => {
                let key = meta
                    .schema_meta
                    .shredded_maps
                    .first()
                    .and_then(|sm| sm.keys.first())
                    .map(|k| k.key.clone())
                    .unwrap_or_else(|| "customer_id".to_owned());
                let _ = reader.scan(&ScanRequest {
                    map_keys: vec![(field.name.clone(), key.clone())],
                    ..Default::default()
                });
                let _ = reader.scan(&ScanRequest {
                    predicates: vec![Predicate::map_key(
                        &field.name,
                        &key,
                        ZonePred::Eq(ZoneValue::Bytes(b"cust_0".to_vec())),
                    )],
                    map_keys: vec![(field.name.clone(), key)],
                    ..Default::default()
                });
            }
            _ => {}
        }
    }
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
            replay_bytes(&data);
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
    ] {
        let data = std::fs::read(corpus_dir.join(name)).expect(name);
        let reader = StatamReader::open(Arc::new(SliceReader::new(data)), None).expect(name);
        reader.verify_full().expect(name);
        reader.scan(&ScanRequest::default()).expect(name);
    }

    for name in ["seed-09-stpk-single", "seed-10-stpk-multi-frame"] {
        let data = std::fs::read(corpus_dir.join(name)).expect(name);
        let pack = ChunkPackReader::open(Arc::new(SliceReader::new(data))).expect(name);
        pack.read_edge_table().expect(name);
        pack.read_hash_table().expect(name);
    }

    for name in ["seed-11-paired-l1-spans", "seed-12-paired-multiturn"] {
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
