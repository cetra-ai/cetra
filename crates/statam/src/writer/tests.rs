use cetra_column::{Field, Utf8Data};

use super::rg_encode::MIN_PARALLEL_RG_ROWS;
use super::*;
use crate::format::{
    COL_FLAG_BLOOM_IN_HOT_TAIL, COL_FLAG_DICT_IN_HOT_TAIL, COL_FLAG_HAS_TRIGRAM_INDEX, Encoding,
    MAX_HOT_TAIL_BYTES,
};
use crate::io::SliceReader;
use crate::payload::pack::{ChunkPackReader, PackWriteOptions};
use crate::reader::{
    InMemoryPackResolver, PackResolver, ScanRequest, StatamReader, SubstringQuery,
};

#[test]
fn l0fast_skips_widerow_dict_probing_and_narrow_trigram_skips_dict() {
    let schema = Schema::try_new_arc(vec![
        Field::new("name", DataType::Utf8, false),
        Field::new("input_preview", DataType::Utf8, false),
        Field::new("output_preview", DataType::Utf8, false),
        Field::new("request_params", DataType::Utf8, false),
        Field::new("status_message", DataType::Utf8, false),
    ])
    .unwrap();

    // Low cardinality on all columns (only 3 distinct values across 64 rows).
    let n = 64usize;
    let vals: Vec<String> = (0..n).map(|i| format!("val_{}", i % 3)).collect();
    let refs: Vec<&str> = vals.iter().map(String::as_str).collect();
    let make_col = || {
        Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(refs.iter().copied()).unwrap()),
                None,
            )
            .unwrap(),
        )
    };
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![make_col(), make_col(), make_col(), make_col(), make_col()],
        n,
    )
    .unwrap();

    // L0Fast: `name` (Core) gets FileDict, WideRow preview/params/status_message MUST NOT.
    let mut opts_l0 = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
    opts_l0.bloom_cols.clear();
    opts_l0.hot_bloom_col = None;
    let mut l0_bytes = Vec::new();
    let mut w0 = StatamWriter::new(
        std::io::Cursor::new(&mut l0_bytes),
        Arc::clone(&schema),
        opts_l0,
        None,
    )
    .unwrap();
    w0.write_batch(&batch).unwrap();
    w0.finish().unwrap();
    let r0 = StatamReader::open(Arc::new(SliceReader::new(l0_bytes)), None).unwrap();
    assert_ne!(
        r0.meta().col_dir[0].col_flags & COL_FLAG_HAS_FILE_DICT,
        0,
        "Core narrow Utf8 col `name` should get FileDict"
    );
    for wide_idx in 1..=4 {
        assert_eq!(
            r0.meta().col_dir[wide_idx].col_flags & COL_FLAG_HAS_FILE_DICT,
            0,
            "L0Fast must NOT probe FileDict on WideRow col {wide_idx}"
        );
    }

    // L1Compact: `name` is Dict-encoded -> must skip narrow trigram index!
    let mut opts_l1 = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
    opts_l1.bloom_cols.clear();
    opts_l1.hot_bloom_col = None;
    let mut l1_bytes = Vec::new();
    let mut w1 = StatamWriter::new(
        std::io::Cursor::new(&mut l1_bytes),
        Arc::clone(&schema),
        opts_l1,
        None,
    )
    .unwrap();
    w1.write_batch(&batch).unwrap();
    w1.finish().unwrap();
    let r1 = StatamReader::open(Arc::new(SliceReader::new(l1_bytes)), None).unwrap();
    assert_eq!(
        r1.meta().col_dir[0].col_flags & COL_FLAG_HAS_TRIGRAM_INDEX,
        0,
        "Dict-encoded `name` must not build NarrowTrigram index"
    );
}

#[test]
fn w4_bypass_writes_uncompressed_raw_pack_frames_without_trigrams() {
    let schema = Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("input", DataType::Utf8, true),
    ])
    .unwrap();

    let pack_id = [0x55u8; 16];
    let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
    opts.payload_cols = vec!["input".to_owned()];
    opts.pack_id = Some(pack_id);

    // 128 rows of high-entropy incompressible 2 KiB ASCII strings -> triggers W4 bypass after 64 chunks.
    let n = 128usize;
    let ids: Vec<i64> = (0..n as i64).collect();
    let mut rng = 0x1234_5678_9abc_def0u64;
    let mut payloads = Vec::with_capacity(n);
    for i in 0..n {
        let mut s = format!("W4_ROW_{i:04}_");
        for _ in 0..2048 {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            s.push((b'!' + ((rng >> 56) as u8 % 90)) as char);
        }
        payloads.push(s);
    }
    let p_refs: Vec<&str> = payloads.iter().map(String::as_str).collect();
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(p_refs).unwrap()),
                    None,
                )
                .unwrap(),
            ),
        ],
        n,
    )
    .unwrap();

    let pack_opts = PackWriteOptions {
        #[cfg(miri)]
        codec: Codec::Lz4,
        #[cfg(not(miri))]
        codec: Codec::Zstd,
        zstd_level: 3,
        target_frame_raw_bytes: 16 * 1024,
        enable_trigram_skip: true,
        compress_toc_lz4: true,
        write_threads: 1,
        emit_hash_table: true,
    };
    let mut pb = ChunkPackBuilder::new(pack_id, pack_opts);
    let mut file_bytes = Vec::new();
    let mut w = StatamWriter::new(
        std::io::Cursor::new(&mut file_bytes),
        Arc::clone(&schema),
        opts,
        Some(&mut pb),
    )
    .unwrap();
    w.write_batch(&batch).unwrap();
    w.finish().unwrap();
    let mut pack_bytes = Vec::new();
    pb.finish(&mut pack_bytes).unwrap();

    let pack_reader =
        Arc::new(ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes))).unwrap());
    let bypass_frames: Vec<_> = pack_reader
        .toc()
        .frames
        .iter()
        .filter(|f| f.filter_len == 0)
        .collect();
    assert!(
        !bypass_frames.is_empty(),
        "W4 bypass must omit trigram blooms (filter_len == 0)"
    );

    // Also verify append_raw_bypass falls back to Codec::None on truly incompressible bytes.
    let mut raw_pb = ChunkPackBuilder::new([0x66u8; 16], pack_opts);
    let mut incomp = vec![0u8; 20 * 1024];
    let mut s = 0x9e37_79b9_7f4a_7c15u64;
    for b in &mut incomp {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        *b = (s >> 24) as u8;
    }
    raw_pb.append_raw_bypass(&incomp).unwrap();
    let mut raw_pack_bytes = Vec::new();
    raw_pb.finish(&mut raw_pack_bytes).unwrap();
    let raw_pack_reader =
        ChunkPackReader::open(Arc::new(SliceReader::new(raw_pack_bytes))).unwrap();
    assert_eq!(raw_pack_reader.toc().frames[0].codec, Codec::None);
    assert_eq!(raw_pack_reader.toc().frames[0].filter_len, 0);

    let mut resolver = InMemoryPackResolver::new();
    resolver.insert(pack_id, pack_reader);
    let resolver: Arc<dyn PackResolver> = Arc::new(resolver);
    let reader = StatamReader::open(
        Arc::new(SliceReader::new(file_bytes)),
        Some(Arc::clone(&resolver)),
    )
    .unwrap();
    reader.verify_full().unwrap();
    let out = reader.scan(&ScanRequest::default()).unwrap();
    assert_eq!(out[0].num_rows(), n);
    assert_eq!(
        out[0].column(1).unwrap().get_scalar(120).unwrap(),
        ScalarValue::Utf8(payloads[120].clone())
    );
}

#[test]
fn hot_tail_stays_within_1mib_and_keeps_small_dicts_when_compressed_fits() {
    // Test with small RG/MB sizes producing thousands of blocks + primary/secondary blooms + small dict.
    let schema = Schema::try_new_arc(vec![
        Field::new("trace_id", DataType::FixedBytes16, false),
        Field::new("span_id", DataType::FixedBytes8, false),
        Field::new("status_code", DataType::Utf8, false),
    ])
    .unwrap();

    let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
    opts.row_group_size = 64;
    opts.mini_block_size = 32;

    // 8,192 rows -> 128 RGs * 2 MBs * 3 cols = 768 blocks (~49 KiB raw block_dir)
    // + unique trace_id/span_id blooms + small status_code FileDict.
    let n = 8_192usize;
    let tids: Vec<[u8; 16]> = (0..n)
        .map(|i| {
            let mut b = [0u8; 16];
            b[0..8].copy_from_slice(&(i as u64).to_le_bytes());
            b
        })
        .collect();
    let sids: Vec<[u8; 8]> = (0..n).map(|i| (i as u64).to_be_bytes()).collect();
    let statuses: Vec<&str> = (0..n)
        .map(|i| if i % 2 == 0 { "OK" } else { "ERR" })
        .collect();

    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(tids), None).unwrap()),
            Column::Flat(FlatColumn::try_new(FlatData::FixedBytes8(sids), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(statuses).unwrap()),
                    None,
                )
                .unwrap(),
            ),
        ],
        n,
    )
    .unwrap();

    let mut file_bytes = Vec::new();
    let mut w = StatamWriter::new(
        std::io::Cursor::new(&mut file_bytes),
        Arc::clone(&schema),
        opts,
        None,
    )
    .unwrap();
    w.write_batch(&batch).unwrap();
    let stats = w.finish().unwrap();
    assert!(
        stats.hot_tail_bytes <= DEFAULT_TAIL_HINT,
        "hot_tail_bytes {} exceeded 128 KiB DEFAULT_TAIL_HINT",
        stats.hot_tail_bytes
    );

    let reader = StatamReader::open(Arc::new(SliceReader::new(file_bytes)), None).unwrap();
    reader.verify_full().unwrap();
    assert_ne!(
        reader.meta().col_dir[0].col_flags & COL_FLAG_BLOOM_IN_HOT_TAIL,
        0
    );
    assert_ne!(
        reader.meta().col_dir[2].col_flags & COL_FLAG_DICT_IN_HOT_TAIL,
        0,
        "small FileDict must remain in Hot Tail when compressed Hot Tail <= 120 KiB"
    );
}

#[test]
fn write_batch_with_empty_prechunked_after_partial_rg_preserves_external_payloads() {
    use crate::payload::pack::PackWriteOptions;
    use crate::reader::{InMemoryPackResolver, PackResolver};

    let schema = Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("input", DataType::Utf8, true),
    ])
    .unwrap();

    let make_batch = |ids: &[i64], payloads: &[&str]| {
        Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(ids.to_vec()), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(payloads.iter().copied()).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            ids.len(),
        )
        .unwrap()
    };

    let p0 = format!("partial_l0_0_{}", "a".repeat(600));
    let p1 = "inline_1".to_owned();
    let p2 = format!("prechunked_empty_2_{}", "b".repeat(700));
    let p3 = format!("prechunked_empty_3_{}", "c".repeat(800));
    let b1 = make_batch(&[0, 1], &[&p0, &p1]);
    let b2 = make_batch(&[2, 3], &[&p2, &p3]);
    let expected = [p0, p1, p2, p3];

    // 1. L0Fast: partial RG via write_batch + write_batch_with_prechunked_refs with empty map.
    let mut l0_opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
    l0_opts.payload_cols = vec!["input".to_owned()];
    l0_opts.row_group_size = 8;
    l0_opts.mini_block_size = 4;

    let mut l0_bytes = Vec::new();
    let mut w0 = StatamWriter::new(
        std::io::Cursor::new(&mut l0_bytes),
        Arc::clone(&schema),
        l0_opts,
        None,
    )
    .unwrap();
    w0.write_batch(&b1).unwrap();
    w0.write_batch_with_prechunked_refs(&b2, &HashMap::new(), None)
        .unwrap();
    w0.finish().unwrap();

    let r0 = StatamReader::open(Arc::new(SliceReader::new(l0_bytes)), None).unwrap();
    r0.verify_full().unwrap();
    let out0 = r0.scan(&ScanRequest::default()).unwrap();
    assert_eq!(out0.len(), 1);
    assert_eq!(out0[0].num_rows(), 4);
    for (idx, exp) in expected.iter().enumerate() {
        assert_eq!(
            out0[0].column(1).unwrap().get_scalar(idx).unwrap(),
            ScalarValue::Utf8(exp.clone())
        );
    }

    // 2. L1Compact with ChunkPackBuilder: partial RG via write_batch + empty prechunked.
    let pack_id = [0x5Au8; 16];
    let mut pb = ChunkPackBuilder::new(
        pack_id,
        PackWriteOptions {
            codec: default_l1_codec(),
            zstd_level: 3,
            target_frame_raw_bytes: 16 * 1024,
            enable_trigram_skip: true,
            compress_toc_lz4: true,
            write_threads: 1,
            emit_hash_table: true,
        },
    );
    let mut l1_opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
    l1_opts.payload_cols = vec!["input".to_owned()];
    l1_opts.row_group_size = 8;
    l1_opts.mini_block_size = 4;
    l1_opts.pack_id = Some(pack_id);

    let mut l1_bytes = Vec::new();
    {
        let mut w1 = StatamWriter::new(
            std::io::Cursor::new(&mut l1_bytes),
            Arc::clone(&schema),
            l1_opts,
            Some(&mut pb),
        )
        .unwrap();
        w1.write_batch(&b1).unwrap();
        w1.write_batch_with_prechunked_refs(&b2, &HashMap::new(), None)
            .unwrap();
        w1.finish().unwrap();
    }
    let mut pack_bytes = Vec::new();
    pb.finish(&mut pack_bytes).unwrap();
    let mut resolver = InMemoryPackResolver::new();
    resolver.insert_bytes(pack_id, pack_bytes).unwrap();
    let resolver: Arc<dyn PackResolver> = Arc::new(resolver);

    let r1 = StatamReader::open(Arc::new(SliceReader::new(l1_bytes)), Some(resolver)).unwrap();
    r1.verify_full().unwrap();
    let out1 = r1.scan(&ScanRequest::default()).unwrap();
    assert_eq!(out1.len(), 1);
    assert_eq!(out1[0].num_rows(), 4);
    for (idx, exp) in expected.iter().enumerate() {
        assert_eq!(
            out1[0].column(1).unwrap().get_scalar(idx).unwrap(),
            ScalarValue::Utf8(exp.clone())
        );
    }
}

#[test]
fn wide_row_file_dict_caps_entries_and_memory_bytes() {
    use crate::format::COL_FLAG_HAS_FILE_DICT;

    let schema = Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("input_preview", DataType::Utf8, true),
        Field::new("output_preview", DataType::Utf8, true),
    ])
    .unwrap();

    let n = 2_048usize;
    let ids: Vec<i64> = (0..n as i64).collect();
    // 512 unique ~600 B previews (~307 KiB > 256 KiB DICT_PASSTHROUGH_MAX_BYTES): must NOT build a FileDict.
    let large_previews: Vec<String> = (0..n)
        .map(|i| format!("preview_{:04}_{}", i % 512, "x".repeat(590)))
        .collect();
    // 64 unique ~16 B previews (~1 KiB <= 256 KiB cap): MAY build a FileDict.
    let small_previews: Vec<String> = (0..n).map(|i| format!("short_{:02}_ok", i % 64)).collect();

    let large_refs: Vec<&str> = large_previews.iter().map(String::as_str).collect();
    let small_refs: Vec<&str> = small_previews.iter().map(String::as_str).collect();
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(large_refs).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(small_refs).unwrap()),
                    None,
                )
                .unwrap(),
            ),
        ],
        n,
    )
    .unwrap();

    let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
    opts.row_group_size = n;
    opts.mini_block_size = 512;

    let mut file_bytes = Vec::new();
    let mut w = StatamWriter::new(
        std::io::Cursor::new(&mut file_bytes),
        Arc::clone(&schema),
        opts,
        None,
    )
    .unwrap();
    w.write_batch(&batch).unwrap();
    w.finish().unwrap();

    let reader = StatamReader::open(Arc::new(SliceReader::new(file_bytes)), None).unwrap();
    assert_eq!(
        reader.meta().col_dir[1].col_flags & COL_FLAG_HAS_FILE_DICT,
        0,
        "WideRow column > 256 KiB (DICT_PASSTHROUGH_MAX_BYTES) dict memory must not build a FileDict"
    );
    assert_ne!(
        reader.meta().col_dir[2].col_flags & COL_FLAG_HAS_FILE_DICT,
        0,
        "compact WideRow column <= 1024 entries and <= 256 KiB may keep a FileDict"
    );
}

fn make_mt_spans_batch(n: usize) -> (Arc<Schema>, Batch) {
    use cetra_column::MapData;

    let schema = Schema::try_new_arc(vec![
        Field::new("status", DataType::Utf8, false),
        Field::new("model", DataType::Utf8, true),
        Field::new("name", DataType::Utf8, false),
        Field::new("start_time", DataType::TimestampNs { utc: true }, false),
        Field::new("metadata", DataType::MapUtf8, true),
        Field::new("trace_id", DataType::FixedBytes16, false),
        Field::new("cost_total", DataType::F64, false),
        Field::new("total_tokens", DataType::I64, false),
        Field::new("span_id", DataType::FixedBytes8, false),
        Field::new("user_id", DataType::Utf8, true),
        Field::new("session_id", DataType::Utf8, true),
        Field::new("duration_ns", DataType::I64, false),
        Field::new("provider", DataType::Utf8, false),
        Field::new("attributes", DataType::MapUtf8, true),
        Field::new("prompt_tokens", DataType::I64, false),
        Field::new("input_preview", DataType::Utf8, true),
        Field::new("all_null_col", DataType::Utf8, true),
        Field::new("const_str_col", DataType::Utf8, false),
        Field::new("input", DataType::Utf8, true),
        Field::new("output", DataType::Utf8, true),
    ])
    .unwrap();

    let statuses: Vec<&str> = (0..n)
        .map(|i| if i % 5 == 0 { "ERR" } else { "OK" })
        .collect();
    let models: Vec<&str> = (0..n).map(|i| ["m-a", "m-b", "m-c"][i % 3]).collect();
    // High-cardinality unique `name` so FileDict is skipped and narrow_trigram_cols activates.
    let names: Vec<String> = (0..n)
        .map(|i| format!("span-op-{i:05}-{}", "x".repeat(24)))
        .collect();
    let start_times: Vec<i64> = (0..n)
        .map(|i| 1_700_000_000_000_000_000 + (i as i64) * 1_000)
        .collect();

    let build_map = |prefix: &str| -> Column {
        let mut offsets = Vec::with_capacity(n + 1);
        offsets.push(0u32);
        let mut keys = Vec::new();
        let mut vals = Vec::new();
        for i in 0..n {
            keys.push(format!("{prefix}.env"));
            vals.push(if i % 2 == 0 {
                "prod".to_owned()
            } else {
                "dev".to_owned()
            });
            keys.push(format!("{prefix}.region"));
            vals.push(format!("r{}", i % 4));
            if i % 10 == 0 {
                keys.push(format!("{prefix}.rare_{i}"));
                vals.push(format!("v_{i}"));
            }
            offsets.push(keys.len() as u32);
        }
        let kd = Utf8Data::try_from_slices(keys.iter().map(String::as_str)).unwrap();
        let vd = Utf8Data::try_from_slices(vals.iter().map(String::as_str)).unwrap();
        Column::Flat(
            FlatColumn::try_new(
                FlatData::MapUtf8(Box::new(MapData::try_new(offsets, kd, vd, None).unwrap())),
                None,
            )
            .unwrap(),
        )
    };

    let trace_ids: Vec<[u8; 16]> = (0..n)
        .map(|i| {
            let mut b = [0u8; 16];
            b[0..8].copy_from_slice(&((i / 4) as u64).to_le_bytes());
            b
        })
        .collect();
    let costs: Vec<f64> = (0..n).map(|i| (i as f64) * 0.001).collect();
    let tokens: Vec<i64> = (0..n).map(|i| 100 + (i as i64 % 500)).collect();
    let span_ids: Vec<[u8; 8]> = (0..n).map(|i| (i as u64).to_be_bytes()).collect();
    let users: Vec<String> = (0..n).map(|i| format!("u_{:02}", i % 16)).collect();
    let sessions: Vec<String> = (0..n).map(|i| format!("s_{:02}", i % 32)).collect();
    let durations: Vec<i64> = (0..n).map(|i| 10_000 + (i as i64 * 7)).collect();
    let providers: Vec<&str> = (0..n).map(|i| ["openai", "anthropic"][i % 2]).collect();
    let prompt_tokens: Vec<i64> = (0..n).map(|i| 50 + (i as i64 % 200)).collect();
    let previews: Vec<String> = (0..n)
        .map(|i| format!("preview-{i:04}-{}", "abc ".repeat(16)))
        .collect();
    let inputs: Vec<String> = (0..n)
        .map(|i| {
            format!(
                "[{{\"role\":\"user\",\"content\":\"msg-{i:04}-{}\"}}]",
                "Z".repeat(600)
            )
        })
        .collect();
    let outputs: Vec<String> = (0..n)
        .map(|i| format!("reply-{i:04}-{}", "W".repeat(400)))
        .collect();

    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(statuses).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(models).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(names.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: start_times,
                        utc: true,
                    },
                    None,
                )
                .unwrap(),
            ),
            build_map("m"),
            Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(trace_ids), None).unwrap()),
            Column::Flat(FlatColumn::try_new(FlatData::F64(costs), None).unwrap()),
            Column::Flat(FlatColumn::try_new(FlatData::I64(tokens), None).unwrap()),
            Column::Flat(FlatColumn::try_new(FlatData::FixedBytes8(span_ids), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(users.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(sessions.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(FlatColumn::try_new(FlatData::I64(durations), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(providers).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            build_map("a"),
            Column::Flat(FlatColumn::try_new(FlatData::I64(prompt_tokens), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(previews.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
            Column::try_new_const(ScalarValue::Null(DataType::Utf8), n).unwrap(),
            Column::try_new_const(ScalarValue::Utf8("constant_tag".to_owned()), n).unwrap(),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(inputs.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(outputs.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
        ],
        n,
    )
    .unwrap();
    (schema, batch)
}

#[test]
fn writer_l1_multithreaded_is_bit_exact_with_single_threaded() {
    let total_n = 1_250usize;
    let (schema, batch) = make_mt_spans_batch(total_n);
    let pack_id = [0xC1u8; 16];

    let run =
        |threads: usize, src: &Batch, split_at: Option<usize>| -> (Vec<u8>, Vec<u8>, WriteStats) {
            let pack_opts = PackWriteOptions {
                write_threads: threads,
                target_frame_raw_bytes: 32 * 1024,
                ..PackWriteOptions::default()
            };
            let mut pb = ChunkPackBuilder::new(pack_id, pack_opts);
            let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
            opts.row_group_size = 1_024;
            opts.mini_block_size = 128;
            opts.pack_id = Some(pack_id);
            opts.write_threads = threads;

            let mut statam_bytes = Vec::new();
            let stats = {
                let mut w = StatamWriter::new(
                    std::io::Cursor::new(&mut statam_bytes),
                    Arc::clone(&schema),
                    opts,
                    Some(&mut pb),
                )
                .unwrap();
                if let Some(mid) = split_at {
                    w.write_batch(&src.slice(0, mid, None).unwrap()).unwrap();
                    w.write_batch(&src.slice(mid, src.num_rows() - mid, None).unwrap())
                        .unwrap();
                } else {
                    w.write_batch(src).unwrap();
                }
                w.finish().unwrap()
            };
            let mut pack_bytes = Vec::new();
            pb.finish(&mut pack_bytes).unwrap();
            (statam_bytes, pack_bytes, stats)
        };

    let (s_0t, p_0t, st_0t) = run(0, &batch, Some(300));
    let (s_1t, p_1t, st_1t) = run(1, &batch, Some(300));
    let (s_4t, p_4t, st_4t) = run(4, &batch, Some(300));
    assert_eq!(st_1t, st_0t);
    assert_eq!(s_1t, s_0t, ".statam L1 output must be bit-exact 1T vs 0T");
    assert_eq!(p_1t, p_0t, ".stpk output must be bit-exact 1T vs 0T");
    assert_eq!(st_1t, st_4t);
    assert_eq!(s_1t, s_4t, ".statam L1 output must be bit-exact 1T vs 4T");
    assert_eq!(p_1t, p_4t, ".stpk output must be bit-exact 1T vs 4T");

    // 1-row tiny-flush parity check with write_threads = 8 (gated to 1T RG path).
    let one_row = batch.slice(0, 1, None).unwrap();
    let (one_s1, one_p1, one_st1) = run(1, &one_row, None);
    let (one_s8, one_p8, one_st8) = run(8, &one_row, None);
    assert_eq!(one_st1, one_st8);
    assert_eq!(one_s1, one_s8, "1-row L1 output must be bit-exact 1T vs 8T");
    assert_eq!(one_p1, one_p8, "1-row L1 .stpk must be bit-exact 1T vs 8T");

    let pack_reader = Arc::new(ChunkPackReader::open(Arc::new(SliceReader::new(p_4t))).unwrap());
    let mut resolver = InMemoryPackResolver::new();
    resolver.insert(pack_id, pack_reader);
    let reader =
        StatamReader::open(Arc::new(SliceReader::new(s_4t)), Some(Arc::new(resolver))).unwrap();
    reader.verify_full().unwrap();
    assert!(reader.meta().rg_dir.len() >= 2);
    assert!(reader.meta().rg_dir[0].row_count >= MIN_PARALLEL_RG_ROWS as u32);
    assert!(!reader.meta().schema_meta.shredded_maps.is_empty());
    let scanned = reader.scan(&ScanRequest::default()).unwrap();
    let total_rows: usize = scanned.iter().map(Batch::num_rows).sum();
    assert_eq!(total_rows, total_n);
}

#[test]
fn writer_l0_multithreaded_is_bit_exact_with_single_threaded() {
    let schema = Schema::try_new_arc(vec![
        Field::new("trace_id", DataType::FixedBytes16, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("duration_ns", DataType::I64, false),
        Field::new("input_preview", DataType::Utf8, true),
        Field::new("input", DataType::Utf8, true),
    ])
    .unwrap();

    // Build >= 1_024 rows and > 3 MiB of L0 local payloads so RG0 exercises
    // parallel RG encoding and LocalL0PayloadBuilder queues >= 6 frames.
    let n = 1_250usize;
    let tids: Vec<[u8; 16]> = (0..n)
        .map(|i| {
            let mut b = [0u8; 16];
            b[0..8].copy_from_slice(&(i as u64).to_le_bytes());
            b
        })
        .collect();
    let statuses: Vec<&str> = (0..n)
        .map(|i| if i % 3 == 0 { "ERR" } else { "OK" })
        .collect();
    let durs: Vec<i64> = (0..n).map(|i| 1000 + i as i64 * 17).collect();
    let previews: Vec<String> = (0..n).map(|i| format!("prev-{i:04}")).collect();
    let inputs: Vec<String> = (0..n)
        .map(|i| {
            format!(
                "{{\"idx\":{i:04},\"data\":\"{}\"}}",
                "payload_chunk_".repeat(200)
            )
        })
        .collect();

    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(tids), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(statuses).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(FlatColumn::try_new(FlatData::I64(durs), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(previews.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(inputs.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
        ],
        n,
    )
    .unwrap();

    let run = |threads: usize, src: &Batch, split_at: Option<usize>| -> (Vec<u8>, WriteStats) {
        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
        opts.row_group_size = 1_024;
        opts.mini_block_size = 128;
        opts.write_threads = threads;
        let mut out = Vec::new();
        let mut w = StatamWriter::new(
            std::io::Cursor::new(&mut out),
            Arc::clone(&schema),
            opts,
            None,
        )
        .unwrap();
        if let Some(mid) = split_at {
            w.write_batch(&src.slice(0, mid, None).unwrap()).unwrap();
            w.write_batch(&src.slice(mid, src.num_rows() - mid, None).unwrap())
                .unwrap();
        } else {
            w.write_batch(src).unwrap();
        }
        let stats = w.finish().unwrap();
        (out, stats)
    };

    let (bytes_0t, stats_0t) = run(0, &batch, Some(300));
    let (bytes_1t, stats_1t) = run(1, &batch, Some(300));
    let (bytes_4t, stats_4t) = run(4, &batch, Some(300));
    assert_eq!(stats_1t, stats_0t);
    assert_eq!(
        bytes_1t, bytes_0t,
        ".statam L0 output must be bit-exact 1T vs 0T"
    );
    assert_eq!(stats_1t, stats_4t);
    assert!(stats_4t.l0_payload_bytes > 0);
    assert_eq!(
        bytes_1t, bytes_4t,
        ".statam L0 output with multi-frame L0 local payloads must be bit-exact 1T vs 4T"
    );

    // 1-row tiny-flush parity check with write_threads = 8 (gated to 1T RG path).
    let one_row = batch.slice(0, 1, None).unwrap();
    let (one_1t, one_s1) = run(1, &one_row, None);
    let (one_8t, one_s8) = run(8, &one_row, None);
    assert_eq!(one_s1, one_s8);
    assert_eq!(one_1t, one_8t, "1-row L0 output must be bit-exact 1T vs 8T");

    let reader = StatamReader::open(Arc::new(SliceReader::new(bytes_4t)), None).unwrap();
    reader.verify_full().unwrap();
    assert!(reader.meta().rg_dir.len() >= 2);
    assert!(reader.meta().rg_dir[0].row_count >= MIN_PARALLEL_RG_ROWS as u32);
    let scanned = reader.scan(&ScanRequest::default()).unwrap();
    let total_rows: usize = scanned.iter().map(Batch::num_rows).sum();
    assert_eq!(total_rows, n);
    assert_eq!(
        scanned[0].column(4).unwrap().get_scalar(0).unwrap(),
        ScalarValue::Utf8(inputs[0].clone())
    );
}

#[test]
fn writer_single_chunk_only_omits_edge_table_and_preserves_substring_parity() {
    let schema = Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("input", DataType::Utf8, true),
    ])
    .unwrap();

    let n = 64usize;
    let ids: Vec<i64> = (0..n as i64).collect();
    let payloads: Vec<String> = (0..n)
        .map(|i| {
            let marker = if i % 16 == 5 {
                "NeedleSingleAlpha"
            } else {
                "OtherSingleText"
            };
            format!(
                "{{\"row\":{i:04},\"tag\":\"{marker}\",\"pad\":\"{}\"}}",
                "x".repeat(420)
            )
        })
        .collect();
    let p_refs: Vec<&str> = payloads.iter().map(String::as_str).collect();
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(p_refs).unwrap()),
                    None,
                )
                .unwrap(),
            ),
        ],
        n,
    )
    .unwrap();

    let pack_id = [0x71u8; 16];
    let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
    assert_eq!(opts.mini_block_size, 8_192);
    opts.pack_id = Some(pack_id);
    opts.payload_cols = vec!["input".to_owned()];

    let mut pb = ChunkPackBuilder::new(pack_id, PackWriteOptions::default());
    let mut statam_bytes = Vec::new();
    {
        let mut w = StatamWriter::new(
            std::io::Cursor::new(&mut statam_bytes),
            Arc::clone(&schema),
            opts,
            Some(&mut pb),
        )
        .unwrap();
        w.write_batch(&batch).unwrap();
        w.finish().unwrap();
    }
    let mut pack_bytes = Vec::new();
    let pack_stats = pb.finish(&mut pack_bytes).unwrap();
    assert_eq!(pack_stats.edge_table_bytes, 0);

    let pack_reader =
        Arc::new(ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes))).unwrap());
    assert_eq!(pack_reader.postscript().edge_table_len, 0);

    let mut resolver = InMemoryPackResolver::new();
    resolver.insert(pack_id, pack_reader);
    let reader = StatamReader::open(
        Arc::new(SliceReader::new(statam_bytes)),
        Some(Arc::new(resolver)),
    )
    .unwrap();
    reader.verify_full().unwrap();

    let like_hits = reader
        .scan(&ScanRequest {
            substrings: vec![SubstringQuery {
                column: "input".to_owned(),
                needle: b"NeedleSingleAlpha".to_vec(),
                case_insensitive: false,
            }],
            ..ScanRequest::default()
        })
        .unwrap();
    let like_rows: usize = like_hits.iter().map(Batch::num_rows).sum();
    assert_eq!(like_rows, 4);

    let ilike_hits = reader
        .scan(&ScanRequest {
            substrings: vec![SubstringQuery {
                column: "input".to_owned(),
                needle: b"needlesinglealpha".to_vec(),
                case_insensitive: true,
            }],
            ..ScanRequest::default()
        })
        .unwrap();
    let ilike_rows: usize = ilike_hits.iter().map(Batch::num_rows).sum();
    assert_eq!(ilike_rows, 4);

    let miss_hits = reader
        .scan(&ScanRequest {
            substrings: vec![SubstringQuery {
                column: "input".to_owned(),
                needle: b"NotPresentAnywhere".to_vec(),
                case_insensitive: false,
            }],
            ..ScanRequest::default()
        })
        .unwrap();
    assert_eq!(miss_hits.iter().map(Batch::num_rows).sum::<usize>(), 0);
}

#[test]
fn writer_mixed_single_and_multi_chunk_zeros_non_boundary_edges_and_matches_seams() {
    let schema = Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("input", DataType::Utf8, true),
    ])
    .unwrap();

    let n = 100usize;
    let ids: Vec<i64> = (0..n as i64).collect();
    let mut payloads = Vec::with_capacity(n);
    for i in 0..96usize {
        payloads.push(format!(
            "{{\"single_id\":{i:04},\"marker\":\"ONLY_SINGLE_{i:04}\",\"pad\":\"{}\"}}",
            "a".repeat(500)
        ));
    }
    // Multi-chunk JSON array rows (including continuation) with a seam-spanning needle:
    // element 0 ends with `SEAM_LEFT`, element 1 starts with `SEAM_RIGHT`, so the seam
    // contains `SEAM_LEFT"},{"role":"assistant","txt":"SEAM_RIGHT`.
    let json_prefix = format!(
        "[{{\"role\":\"user\",\"txt\":\"{}SEAM_LEFT\"}}",
        "u".repeat(300)
    );
    payloads.push(format!(
        "{json_prefix},{{\"role\":\"assistant\",\"txt\":\"SEAM_RIGHT{}\"}}]",
        "v".repeat(300)
    ));
    payloads.push(format!(
        "{json_prefix},{{\"role\":\"assistant\",\"txt\":\"SEAM_RIGHT_CONT{}\"}}]",
        "w".repeat(300)
    ));
    // Also add > MAX_CHUNK (~80 KiB) plain-text multi-chunk rows spanning CDC seams.
    let big_a = format!("{}BIG_CDC_PART_A", "c".repeat(45_000));
    let big_b = format!("BIG_CDC_PART_B{}", "d".repeat(45_000));
    payloads.push(format!("{big_a}{big_b}"));
    payloads.push(format!(
        "{{\"single_tail\":true,\"pad\":\"{}\"}}",
        "z".repeat(450)
    ));

    let p_refs: Vec<&str> = payloads.iter().map(String::as_str).collect();
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(p_refs).unwrap()),
                    None,
                )
                .unwrap(),
            ),
        ],
        n,
    )
    .unwrap();

    let pack_id = [0x72u8; 16];
    let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
    opts.pack_id = Some(pack_id);
    opts.payload_cols = vec!["input".to_owned()];

    let mut pb = ChunkPackBuilder::new(pack_id, PackWriteOptions::default());
    let mut statam_bytes = Vec::new();
    {
        let mut w = StatamWriter::new(
            std::io::Cursor::new(&mut statam_bytes),
            Arc::clone(&schema),
            opts,
            Some(&mut pb),
        )
        .unwrap();
        w.write_batch(&batch).unwrap();
        w.finish().unwrap();
    }
    let mut pack_bytes = Vec::new();
    let pack_stats = pb.finish(&mut pack_bytes).unwrap();
    assert!(pack_stats.edge_table_bytes > 0);

    let pack_reader =
        Arc::new(ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes))).unwrap());
    let flat_edges = pack_reader.read_edge_table_flat().unwrap();
    assert_eq!(flat_edges.len(), pack_stats.chunk_count as usize);

    // First 96 chunks belong exclusively to single-chunk rows -> edges must be zeroed.
    let mut zeroed_count = 0usize;
    let mut non_zero_count = 0usize;
    for cid in 0..pack_stats.chunk_count {
        if flat_edges.head(cid).is_empty() && flat_edges.tail(cid).is_empty() {
            zeroed_count += 1;
        } else {
            non_zero_count += 1;
        }
    }
    assert!(
        zeroed_count >= 96,
        "expected >= 96 zeroed non-boundary chunks, got {zeroed_count}"
    );
    assert!(
        non_zero_count >= 4,
        "expected boundary chunks to keep non-empty edges"
    );

    // Compare edge_table_bytes against a pack with full untracked edges for the same chunks.
    let mut baseline_pb = ChunkPackBuilder::new(pack_id, PackWriteOptions::default());
    for cid in 0..pack_stats.chunk_count {
        let chunk = pack_reader.read_chunks(&[cid]).unwrap();
        baseline_pb.intern_chunk(&chunk[&cid]).unwrap();
    }
    let mut baseline_bytes = Vec::new();
    let baseline_stats = baseline_pb.finish(&mut baseline_bytes).unwrap();
    assert!(
        pack_stats.edge_table_bytes < baseline_stats.edge_table_bytes,
        "tracked mixed edge_table_bytes ({}) should be smaller than untracked ({})",
        pack_stats.edge_table_bytes,
        baseline_stats.edge_table_bytes
    );

    let mut resolver = InMemoryPackResolver::new();
    resolver.insert(pack_id, pack_reader);
    let reader = StatamReader::open(
        Arc::new(SliceReader::new(statam_bytes)),
        Some(Arc::new(resolver)),
    )
    .unwrap();
    reader.verify_full().unwrap();

    // Seam-spanning LIKE & ILIKE across JSON array element boundaries must match rows 96 & 97.
    let seam_needle = b"SEAM_LEFT\"},{\"role\":\"assistant\",\"txt\":\"SEAM_RIGHT";
    let seam_like = reader
        .scan(&ScanRequest {
            substrings: vec![SubstringQuery {
                column: "input".to_owned(),
                needle: seam_needle.to_vec(),
                case_insensitive: false,
            }],
            ..ScanRequest::default()
        })
        .unwrap();
    assert_eq!(seam_like.iter().map(Batch::num_rows).sum::<usize>(), 2);

    let seam_ilike_needle = b"seam_left\"},{\"ROLE\":\"ASSISTANT\",\"txt\":\"seam_right";
    let seam_ilike = reader
        .scan(&ScanRequest {
            substrings: vec![SubstringQuery {
                column: "input".to_owned(),
                needle: seam_ilike_needle.to_vec(),
                case_insensitive: true,
            }],
            ..ScanRequest::default()
        })
        .unwrap();
    assert_eq!(seam_ilike.iter().map(Batch::num_rows).sum::<usize>(), 2);

    // Large multi-chunk row (row 98) must match at both interior and across any CDC boundary.
    let big_like = reader
        .scan(&ScanRequest {
            substrings: vec![SubstringQuery {
                column: "input".to_owned(),
                needle: b"BIG_CDC_PART_ABIG_CDC_PART_B".to_vec(),
                case_insensitive: false,
            }],
            ..ScanRequest::default()
        })
        .unwrap();
    assert_eq!(big_like.iter().map(Batch::num_rows).sum::<usize>(), 1);
}

#[test]
fn b5_streaming_writer_nonzero_cursor_offset_patches_header_and_leaves_eof() {
    let schema = Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("msg", DataType::Utf8, false),
    ])
    .unwrap();
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::I64(vec![10, 20, 30, 40]), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["a", "b", "c", "d"]).unwrap()),
                    None,
                )
                .unwrap(),
            ),
        ],
        4,
    )
    .unwrap();

    let prefix = b"PREFIX_HEADER_BYTES_12345";
    let mut backing = prefix.to_vec();
    let mut cursor = std::io::Cursor::new(&mut backing);
    cursor.seek(SeekFrom::End(0)).unwrap();

    let mut opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast);
    opts.row_group_size = 2;
    opts.mini_block_size = 2;
    let mut writer = StatamWriter::new(&mut cursor, Arc::clone(&schema), opts, None).unwrap();
    writer.write_batch(&batch).unwrap();
    let stats = writer.finish().unwrap();

    assert_eq!(stats.num_row_groups, 2);
    assert_eq!(
        cursor.position(),
        prefix.len() as u64 + stats.total_bytes,
        "finish() must leave output cursor positioned at EOF"
    );
    assert_eq!(&backing[..prefix.len()], prefix);
    assert_eq!(
        backing.len() as u64,
        prefix.len() as u64 + stats.total_bytes
    );

    let statam_slice = backing[prefix.len()..].to_vec();
    let reader = StatamReader::open(Arc::new(SliceReader::new(statam_slice)), None).unwrap();
    reader.verify_full().unwrap();
    let batches = reader.scan(&ScanRequest::default()).unwrap();
    let total_rows: usize = batches.iter().map(Batch::num_rows).sum();
    assert_eq!(total_rows, 4);
}

#[test]
fn b6_encoded_rg_byte_cap_splits_incompressible_keeps_compressible_and_validates_bounds() {
    let schema = Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("input", DataType::Utf8, false),
    ])
    .unwrap();

    // 1. Invalid max_row_group_bytes (0 or > MAX_BATCH_DECODED_BYTES / 2) returns BoundsExceeded
    //    before writing anything to `out`.
    for bad_cap in [0usize, MAX_BATCH_DECODED_BYTES / 2 + 1] {
        let mut opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast);
        opts.max_row_group_bytes = bad_cap;
        let mut buf = Vec::new();
        let err = StatamWriter::new(
            std::io::Cursor::new(&mut buf),
            Arc::clone(&schema),
            opts,
            None,
        )
        .unwrap_err();
        assert!(matches!(err, StatamError::BoundsExceeded { .. }));
        assert!(buf.is_empty());
    }

    // Deterministic pseudo-random printable ASCII generator (incompressible).
    let make_random_ascii = |seed: u64, len: usize| -> String {
        let mut s = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut out = String::with_capacity(len);
        for _ in 0..len {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            out.push((b'!' + (s % 94) as u8) as char);
        }
        out
    };

    // 2a. Uniform incompressible L0 batch > max_row_group_bytes (256 KiB) auto-splits into multiple valid RGs.
    let n_uniform = 32usize;
    let uniform_payloads: Vec<String> = (0..n_uniform)
        .map(|i| make_random_ascii(100 + i as u64, 32 * 1024)) // 32 * 32 KiB = 1 MiB incompressible
        .collect();
    let uniform_batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(FlatData::I64((0..n_uniform as i64).collect()), None).unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(
                            uniform_payloads
                                .iter()
                                .map(String::as_str)
                                .collect::<Vec<_>>(),
                        )
                        .unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
        ],
        n_uniform,
    )
    .unwrap();

    for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
        let mut opts = WriteOptions::for_table(TableKind::Generic, profile);
        opts.payload_cols = vec!["input".to_owned()];
        opts.row_group_size = 131_072;
        opts.max_row_group_bytes = 256 * 1024;
        let mut out = Vec::new();
        let mut w = StatamWriter::new(
            std::io::Cursor::new(&mut out),
            Arc::clone(&schema),
            opts,
            None,
        )
        .unwrap();
        w.write_batch(&uniform_batch).unwrap();
        let stats = w.finish().unwrap();
        assert!(
            stats.num_row_groups >= 3,
            "expected >= 3 RGs for 1 MiB incompressible batch with 256 KiB cap ({profile:?}), got {}",
            stats.num_row_groups
        );
        let reader = StatamReader::open(Arc::new(SliceReader::new(out)), None).unwrap();
        reader.verify_full().unwrap();
        let batches = reader.scan(&ScanRequest::default()).unwrap();
        let mut read_back = Vec::new();
        for b in &batches {
            let c = b.compact(None).unwrap();
            let fc = c.column(1).unwrap().materialize(None).unwrap();
            let FlatData::Utf8(u) = fc.data() else {
                panic!("expected Utf8");
            };
            for r in 0..u.len() {
                read_back.push(u.get(r).unwrap().to_owned());
            }
        }
        assert_eq!(read_back, uniform_payloads);
    }

    // 2b. Skewed incompressible batch (tiny rows + 200 KiB giant rows) splits cleanly and reads bit-exact.
    let mut skewed_payloads = Vec::new();
    for i in 0..40usize {
        if i == 10 || i == 25 || i == 35 {
            skewed_payloads.push(make_random_ascii(500 + i as u64, 200 * 1024));
        } else {
            skewed_payloads.push(make_random_ascii(500 + i as u64, 512));
        }
    }
    let skewed_batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::I64((0..skewed_payloads.len() as i64).collect()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(
                            skewed_payloads
                                .iter()
                                .map(String::as_str)
                                .collect::<Vec<_>>(),
                        )
                        .unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
        ],
        skewed_payloads.len(),
    )
    .unwrap();
    let mut skew_opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast);
    skew_opts.payload_cols = vec!["input".to_owned()];
    skew_opts.max_row_group_bytes = 256 * 1024;
    let mut skew_out = Vec::new();
    let mut ws = StatamWriter::new(
        std::io::Cursor::new(&mut skew_out),
        Arc::clone(&schema),
        skew_opts,
        None,
    )
    .unwrap();
    ws.write_batch(&skewed_batch).unwrap();
    let skew_stats = ws.finish().unwrap();
    assert!(skew_stats.num_row_groups >= 2);
    let skew_reader = StatamReader::open(Arc::new(SliceReader::new(skew_out)), None).unwrap();
    skew_reader.verify_full().unwrap();
    let mut skew_read_back = Vec::new();
    for b in &skew_reader.scan(&ScanRequest::default()).unwrap() {
        let c = b.compact(None).unwrap();
        let fc = c.column(1).unwrap().materialize(None).unwrap();
        let FlatData::Utf8(u) = fc.data() else {
            panic!("expected Utf8");
        };
        for r in 0..u.len() {
            skew_read_back.push(u.get(r).unwrap().to_owned());
        }
    }
    assert_eq!(skew_read_back, skewed_payloads);

    // 3. Highly compressible payload (raw 1.6 MiB >> 256 KiB cap, compressed << 256 KiB) stays 1 RG!
    let n_comp = 200usize;
    let comp_payloads: Vec<String> = (0..n_comp)
        .map(|i| {
            format!(
                "row-{i:04}-{}",
                "COMPRESSIBLE_AGENT_TRAJECTORY_".repeat(260)
            )
        })
        .collect();
    let comp_batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(FlatData::I64((0..n_comp as i64).collect()), None).unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(
                            comp_payloads.iter().map(String::as_str).collect::<Vec<_>>(),
                        )
                        .unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
        ],
        n_comp,
    )
    .unwrap();
    let mut comp_opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast);
    comp_opts.payload_cols = vec!["input".to_owned()];
    comp_opts.max_row_group_bytes = 256 * 1024;
    let mut comp_out = Vec::new();
    let mut wc = StatamWriter::new(
        std::io::Cursor::new(&mut comp_out),
        Arc::clone(&schema),
        comp_opts,
        None,
    )
    .unwrap();
    wc.write_batch(&comp_batch).unwrap();
    let comp_stats = wc.finish().unwrap();
    assert_eq!(
        comp_stats.num_row_groups, 1,
        "highly compressible 1.6 MiB raw payload should stay 1 RG under 256 KiB encoded cap"
    );
}

#[test]
fn m18_m23_rejects_empty_schema_duplicate_sort_cols_and_unifies_l1_payload_codec() {
    // 1. Empty schema rejected eagerly in StatamWriter::new.
    let empty_schema = Schema::try_new_arc(vec![]).unwrap();
    let mut buf = Vec::new();
    assert!(matches!(
        StatamWriter::new(
            std::io::Cursor::new(&mut buf),
            empty_schema,
            WriteOptions::default(),
            None,
        ),
        Err(StatamError::SchemaMismatch { .. })
    ));
    assert!(buf.is_empty());

    // 2. Duplicate sort_cols rejected eagerly in StatamWriter::new.
    let schema = Schema::try_new_arc(vec![
        Field::new("ts", DataType::I64, false),
        Field::new("input", DataType::Utf8, false),
    ])
    .unwrap();
    let dup_opts = WriteOptions {
        sort_cols: vec![0, 0],
        ..WriteOptions::default()
    };
    assert!(matches!(
        StatamWriter::new(
            std::io::Cursor::new(&mut buf),
            Arc::clone(&schema),
            dup_opts,
            None,
        ),
        Err(StatamError::SchemaMismatch { .. })
    ));
    assert!(buf.is_empty());

    // 3. m23: L1Compact PayloadRefs codec selection matches between direct (single full RG)
    //    and buffered (multi-batch) paths.
    let n = 16usize;
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(FlatData::I64((0..n as i64).collect()), None).unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(
                            (0..n)
                                .map(|i| format!("short-inline-payload-value-{i:04}-repeat-repeat"))
                                .collect::<Vec<_>>()
                                .iter()
                                .map(String::as_str)
                                .collect::<Vec<_>>(),
                        )
                        .unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
        ],
        n,
    )
    .unwrap();

    let pack_id = [0xAAu8; 16];
    let mut opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L1Compact);
    opts.payload_cols = vec!["input".to_owned()];
    opts.pack_id = Some(pack_id);
    opts.row_group_size = n;
    opts.mini_block_size = n;

    let mut direct_bytes = Vec::new();
    let mut pb_direct = ChunkPackBuilder::new(pack_id, PackWriteOptions::default());
    {
        let mut wd = StatamWriter::new(
            std::io::Cursor::new(&mut direct_bytes),
            Arc::clone(&schema),
            opts.clone(),
            Some(&mut pb_direct),
        )
        .unwrap();
        wd.write_batch(&batch).unwrap();
        wd.finish().unwrap();
    }

    let mut buffered_bytes = Vec::new();
    let mut pb_buf = ChunkPackBuilder::new(pack_id, PackWriteOptions::default());
    {
        let mut wb = StatamWriter::new(
            std::io::Cursor::new(&mut buffered_bytes),
            Arc::clone(&schema),
            opts,
            Some(&mut pb_buf),
        )
        .unwrap();
        let b0 = Batch::try_new_all(
            Arc::clone(&schema),
            batch
                .columns()
                .iter()
                .map(|c| c.slice(0, 8, None).unwrap())
                .collect(),
            8,
        )
        .unwrap();
        let b1 = Batch::try_new_all(
            Arc::clone(&schema),
            batch
                .columns()
                .iter()
                .map(|c| c.slice(8, 8, None).unwrap())
                .collect(),
            8,
        )
        .unwrap();
        wb.write_batch(&b0).unwrap();
        wb.write_batch(&b1).unwrap();
        wb.finish().unwrap();
    }

    let meta_d =
        crate::reader::StatamFileMeta::read_from(&SliceReader::new(direct_bytes), None).unwrap();
    let meta_b =
        crate::reader::StatamFileMeta::read_from(&SliceReader::new(buffered_bytes), None).unwrap();
    let blk_d = meta_d.rg_blocks(1, 0)[0];
    let blk_b = meta_b.rg_blocks(1, 0)[0];
    assert_eq!(blk_d.encoding, Encoding::PayloadRefs);
    assert_eq!(blk_b.encoding, Encoding::PayloadRefs);
    assert_eq!(blk_d.codec, blk_b.codec);

    // 4. M18: Oversized uncompressed Hot Tail (even when LZ4 compressed size is small)
    //    spills primary_bloom, dicts, and secondary blooms cleanly so est_raw_len + POSTSCRIPT_LEN
    //    never exceeds MAX_HOT_TAIL_BYTES.
    let mut primary = Some((0usize, vec![0u8; 64 * 1024], true));
    let mut dicts = vec![(1usize, vec![0u8; 4 * 1024], false)];
    let mut sec_blooms = vec![(2usize, vec![0u8; 4 * 1024], false)];
    decide_hot_tail_placement(
        32 * 1024,                      // tiny LZ4-compressed baseline
        MAX_HOT_TAIL_BYTES - 10 * 1024, // nearly full uncompressed baseline
        0,
        &mut primary,
        &mut dicts,
        &mut sec_blooms,
    );
    assert!(
        !primary.as_ref().unwrap().2,
        "primary bloom must spill when raw Hot Tail would exceed MAX_HOT_TAIL_BYTES"
    );
    assert!(
        dicts[0].2,
        "4 KiB dict still fits within remaining raw budget"
    );
    assert!(
        sec_blooms[0].2,
        "4 KiB secondary bloom still fits within remaining raw budget"
    );

    let mut no_primary = None;
    let mut dicts_tight = vec![(1usize, vec![0u8; 4 * 1024], false)];
    let mut sec_tight = vec![(2usize, vec![0u8; 4 * 1024], false)];
    decide_hot_tail_placement(
        32 * 1024,
        MAX_HOT_TAIL_BYTES - POSTSCRIPT_LEN - 5 * 1024,
        0,
        &mut no_primary,
        &mut dicts_tight,
        &mut sec_tight,
    );
    assert!(dicts_tight[0].2);
    assert!(
        !sec_tight[0].2,
        "secondary bloom must spill when raw Hot Tail + POSTSCRIPT_LEN would exceed MAX_HOT_TAIL_BYTES"
    );
}

#[test]
fn pass21_non_payload_compressed_rg_sizing() {
    use cetra_column::{Bitmap, DictColumn, EventsData, LinksData, MapData, RleColumn};

    let make_random_ascii = |seed: u64, len: usize| -> String {
        let mut s = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut out = String::with_capacity(len);
        for _ in 0..len {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            out.push((b'!' + (s % 94) as u8) as char);
        }
        out
    };

    let write_and_verify =
        |schema: &Arc<Schema>, batches: &[Batch], opts: WriteOptions| -> (WriteStats, Vec<Batch>) {
            let mut out = Vec::new();
            let mut w = StatamWriter::new(
                std::io::Cursor::new(&mut out),
                Arc::clone(schema),
                opts,
                None,
            )
            .unwrap();
            for b in batches {
                w.write_batch(b).unwrap();
            }
            let stats = w.finish().unwrap();
            let reader = StatamReader::open(Arc::new(SliceReader::new(out)), None).unwrap();
            reader.verify_full().unwrap();
            let scanned = reader.scan(&ScanRequest::default()).unwrap();
            (stats, scanned)
        };

    let collect_all_scalars = |batches: &[Batch], col_idx: usize| -> Vec<ScalarValue> {
        let mut out = Vec::new();
        for b in batches {
            let c = b.compact(None).unwrap();
            let col = c.column(col_idx).unwrap();
            for r in 0..c.num_rows() {
                out.push(col.get_scalar(r).unwrap());
            }
        }
        out
    };

    // 1. Compressible non-payload single batch (1.6 MiB raw repetitive Utf8 + 95%-NULL I64)
    //    under max_row_group_bytes = 256 KiB stays 1 RG (L0Fast & L1Compact) and round-trips bit-exact.
    let comp_schema = Schema::try_new_arc(vec![
        Field::new("text", DataType::Utf8, false),
        Field::new("sparse_num", DataType::I64, true),
    ])
    .unwrap();
    let n_comp = 200usize;
    let comp_texts: Vec<String> = (0..n_comp)
        .map(|i| {
            format!(
                "row-{i:04}-{}",
                "COMPRESSIBLE_NON_PAYLOAD_PATTERN_".repeat(245)
            )
        })
        .collect();
    let mut sparse_vals = vec![0i64; n_comp];
    let mut sparse_bm = Bitmap::new_zeroed(n_comp);
    for i in (0..n_comp).step_by(20) {
        sparse_vals[i] = (i as i64) * 17 + 3;
        let _ = sparse_bm.set(i, true);
    }
    let comp_batch = Batch::try_new_all(
        Arc::clone(&comp_schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(comp_texts.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(FlatColumn::try_new(FlatData::I64(sparse_vals), Some(sparse_bm)).unwrap()),
        ],
        n_comp,
    )
    .unwrap();
    assert!(
        estimate_column_bytes(comp_batch.column(0).unwrap()) > 1_500_000,
        "expected > 1.5 MiB raw repetitive Utf8"
    );

    let expected_comp_texts = collect_all_scalars(std::slice::from_ref(&comp_batch), 0);
    let expected_comp_nums = collect_all_scalars(std::slice::from_ref(&comp_batch), 1);

    for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
        let mut opts = WriteOptions::for_table(TableKind::Generic, profile);
        opts.max_row_group_bytes = 256 * 1024;
        let (stats, scanned) =
            write_and_verify(&comp_schema, std::slice::from_ref(&comp_batch), opts);
        assert_eq!(
            stats.num_row_groups, 1,
            "compressible 1.6 MiB non-payload single batch must stay 1 RG ({profile:?})"
        );
        assert_eq!(collect_all_scalars(&scanned, 0), expected_comp_texts);
        assert_eq!(collect_all_scalars(&scanned, 1), expected_comp_nums);
    }

    // 2. Compressible non-payload streamed small batches (20 x 80 KiB) stays 1 RG.
    let small_batches: Vec<Batch> = (0..20)
        .map(|chunk| comp_batch.slice(chunk * 10, 10, None).unwrap())
        .collect();
    for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
        let mut opts = WriteOptions::for_table(TableKind::Generic, profile);
        opts.max_row_group_bytes = 256 * 1024;
        let (stats, scanned) = write_and_verify(&comp_schema, &small_batches, opts);
        assert_eq!(
            stats.num_row_groups, 1,
            "compressible 1.6 MiB streamed as 20 x 80 KiB must stay 1 RG ({profile:?})"
        );
        assert_eq!(collect_all_scalars(&scanned, 0), expected_comp_texts);
        assert_eq!(collect_all_scalars(&scanned, 1), expected_comp_nums);
    }

    // 3. Sparse 95%-NULL wide micro-batches (B3 + M2): 50 nullable I64 columns at 95% NULL
    //    streamed in 250 x 100-row micro-batches (< 1024 B/col/batch, ~10 MiB raw) stays 1 RG!
    let wide_cols = 50usize;
    let wide_schema = Schema::try_new_arc(
        (0..wide_cols)
            .map(|c| Field::new(format!("c_{c:02}"), DataType::I64, true))
            .collect(),
    )
    .unwrap();
    let num_micro_batches = 250usize;
    let rows_per_micro = 100usize;
    let mut micro_batches = Vec::with_capacity(num_micro_batches);
    for b_idx in 0..num_micro_batches {
        let mut cols = Vec::with_capacity(wide_cols);
        for c_idx in 0..wide_cols {
            let mut vals = vec![0i64; rows_per_micro];
            let mut bm = Bitmap::new_zeroed(rows_per_micro);
            for (r, slot) in vals.iter_mut().enumerate() {
                let global_r = b_idx * rows_per_micro + r;
                if (global_r + c_idx).is_multiple_of(20) {
                    *slot = (global_r as i64) * 100 + (c_idx as i64);
                    let _ = bm.set(r, true);
                }
            }
            cols.push(Column::Flat(
                FlatColumn::try_new(FlatData::I64(vals), Some(bm)).unwrap(),
            ));
        }
        micro_batches
            .push(Batch::try_new_all(Arc::clone(&wide_schema), cols, rows_per_micro).unwrap());
    }
    assert!(
        estimate_column_bytes(micro_batches[0].column(0).unwrap()) < 1024,
        "each micro-batch column must be < 1024 B to exercise M2 bypass lowering"
    );
    let mut wide_opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast);
    wide_opts.max_row_group_bytes = 2 * 1024 * 1024;
    let (wide_stats, wide_scanned) = write_and_verify(&wide_schema, &micro_batches, wide_opts);
    assert_eq!(
        wide_stats.num_row_groups, 1,
        "250 x 100-row 95%-NULL wide micro-batches (~10 MiB raw) must stay 1 RG"
    );
    assert_eq!(
        wide_scanned.iter().map(Batch::num_rows).sum::<usize>(),
        num_micro_batches * rows_per_micro
    );
    assert_eq!(
        collect_all_scalars(&wide_scanned, 0),
        collect_all_scalars(&micro_batches, 0)
    );
    assert_eq!(
        collect_all_scalars(&wide_scanned, wide_cols - 1),
        collect_all_scalars(&micro_batches, wide_cols - 1)
    );

    // 4. Incompressible non-payload uniform (1 MiB pseudo-random Utf8 + I64 under 256 KiB cap)
    //    splits into >= 3 RGs and round-trips bit-exact.
    let incomp_schema = Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("blob", DataType::Utf8, false),
    ])
    .unwrap();
    let n_incomp = 32usize;
    let incomp_strings: Vec<String> = (0..n_incomp)
        .map(|i| make_random_ascii(1000 + i as u64, 32 * 1024))
        .collect();
    let incomp_ids: Vec<i64> = (0..n_incomp)
        .map(|i| ((i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)) as i64)
        .collect();
    let incomp_batch = Batch::try_new_all(
        Arc::clone(&incomp_schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::I64(incomp_ids), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(incomp_strings.iter().map(String::as_str))
                            .unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
        ],
        n_incomp,
    )
    .unwrap();
    let exp_incomp_blobs = collect_all_scalars(std::slice::from_ref(&incomp_batch), 1);
    for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
        let mut opts = WriteOptions::for_table(TableKind::Generic, profile);
        opts.max_row_group_bytes = 256 * 1024;
        let (stats, scanned) =
            write_and_verify(&incomp_schema, std::slice::from_ref(&incomp_batch), opts);
        assert!(
            stats.num_row_groups >= 3,
            "1 MiB incompressible non-payload must split into >= 3 RGs ({profile:?}), got {}",
            stats.num_row_groups
        );
        assert_eq!(collect_all_scalars(&scanned, 1), exp_incomp_blobs);
    }

    // 5. Skewed incompressible non-payload: 400 tiny 16 B strings + 4 x 200 KiB giant random strings
    //    under 256 KiB cap splits into >= 3 RGs and round-trips bit-exact.
    let mut skewed_strs = Vec::with_capacity(404);
    for i in 0..404usize {
        if i == 80 || i == 180 || i == 280 || i == 380 {
            skewed_strs.push(make_random_ascii(5000 + i as u64, 200 * 1024));
        } else {
            skewed_strs.push(format!("tiny_{i:011}"));
        }
    }
    let skewed_batch = Batch::try_new_all(
        Arc::clone(&incomp_schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(FlatData::I64((0..skewed_strs.len() as i64).collect()), None)
                    .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(skewed_strs.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
        ],
        skewed_strs.len(),
    )
    .unwrap();
    let exp_skewed_blobs = collect_all_scalars(std::slice::from_ref(&skewed_batch), 1);
    let mut skew_opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast);
    skew_opts.max_row_group_bytes = 256 * 1024;
    let (skew_stats, skew_scanned) = write_and_verify(
        &incomp_schema,
        std::slice::from_ref(&skewed_batch),
        skew_opts,
    );
    assert!(
        skew_stats.num_row_groups >= 3,
        "skewed 4 x 200 KiB incompressible non-payload must split into >= 3 RGs, got {}",
        skew_stats.num_row_groups
    );
    assert_eq!(collect_all_scalars(&skew_scanned, 1), exp_skewed_blobs);

    // 6. M1: Incompressible Links (random trace_ids/span_ids, empty attributes) & Events (random times)
    //    split into >= 3 RGs under 256 KiB cap; compressible List<I64> + MapUtf8 stays 1 RG.
    let links_schema =
        Schema::try_new_arc(vec![Field::new("links", DataType::Links, false)]).unwrap();
    let link_rows = 4_200usize;
    let links_per_row = 10usize;
    let total_links = link_rows * links_per_row;
    let mut l_offs = Vec::with_capacity(link_rows + 1);
    l_offs.push(0u32);
    for r in 1..=link_rows {
        l_offs.push((r * links_per_row) as u32);
    }
    let mut rng = 0x1357_9bdf_2468_ace0u64;
    let mut l_tids = Vec::with_capacity(total_links);
    let mut l_sids = Vec::with_capacity(total_links);
    for _ in 0..total_links {
        let mut tid = [0u8; 16];
        for w in tid.as_chunks_mut::<8>().0 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            *w = rng.to_le_bytes();
        }
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        l_tids.push(tid);
        l_sids.push(rng.to_le_bytes());
    }
    let l_attrs = Utf8Data::try_from_slices(std::iter::repeat_n("", total_links)).unwrap();
    let links_batch = Batch::try_new_all(
        Arc::clone(&links_schema),
        vec![Column::Flat(
            FlatColumn::try_new(
                FlatData::Links(Box::new(
                    LinksData::try_new(l_offs, l_tids, l_sids, l_attrs, None).unwrap(),
                )),
                None,
            )
            .unwrap(),
        )],
        link_rows,
    )
    .unwrap();
    let mut links_opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast);
    links_opts.max_row_group_bytes = 256 * 1024;
    let (links_stats, links_scanned) = write_and_verify(
        &links_schema,
        std::slice::from_ref(&links_batch),
        links_opts,
    );
    assert!(
        links_stats.num_row_groups >= 3,
        "incompressible ~1 MiB Links (random trace/span IDs, empty attrs) must split into >= 3 RGs, got {}",
        links_stats.num_row_groups
    );
    assert_eq!(
        links_scanned.iter().map(Batch::num_rows).sum::<usize>(),
        link_rows
    );
    assert_eq!(
        collect_all_scalars(&links_scanned, 0),
        collect_all_scalars(std::slice::from_ref(&links_batch), 0)
    );

    let events_schema =
        Schema::try_new_arc(vec![Field::new("events", DataType::Events, false)]).unwrap();
    let ev_rows = 4_000usize;
    let ev_per_row = 32usize;
    let total_events = ev_rows * ev_per_row; // 128,000 * 8 B = 1 MiB random times
    let mut e_offs = Vec::with_capacity(ev_rows + 1);
    e_offs.push(0u32);
    for r in 1..=ev_rows {
        e_offs.push((r * ev_per_row) as u32);
    }
    let mut e_times = Vec::with_capacity(total_events);
    for _ in 0..total_events {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        e_times.push(rng as i64);
    }
    let e_names = Utf8Data::try_from_slices(std::iter::repeat_n("ev", total_events)).unwrap();
    let e_attrs = Utf8Data::try_from_slices(std::iter::repeat_n("", total_events)).unwrap();
    let events_batch = Batch::try_new_all(
        Arc::clone(&events_schema),
        vec![Column::Flat(
            FlatColumn::try_new(
                FlatData::Events(Box::new(
                    EventsData::try_new(e_offs, e_times, e_names, e_attrs, None).unwrap(),
                )),
                None,
            )
            .unwrap(),
        )],
        ev_rows,
    )
    .unwrap();
    let mut events_opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast);
    events_opts.max_row_group_bytes = 256 * 1024;
    let (events_stats, events_scanned) = write_and_verify(
        &events_schema,
        std::slice::from_ref(&events_batch),
        events_opts,
    );
    assert!(
        events_stats.num_row_groups >= 3,
        "incompressible ~1 MiB Events (random times, repetitive names/attrs) must split into >= 3 RGs, got {}",
        events_stats.num_row_groups
    );
    assert_eq!(
        events_scanned.iter().map(Batch::num_rows).sum::<usize>(),
        ev_rows
    );

    let nested_comp_schema = Schema::try_new_arc(vec![
        Field::new("nums", DataType::List(Box::new(DataType::I64)), false),
        Field::new("attrs", DataType::MapUtf8, false),
    ])
    .unwrap();
    let nc_rows = 2_000usize;
    let list_per_row = 64usize;
    let mut list_offs = Vec::with_capacity(nc_rows + 1);
    list_offs.push(0u32);
    for r in 1..=nc_rows {
        list_offs.push((r * list_per_row) as u32);
    }
    let list_vals = vec![42i64; nc_rows * list_per_row]; // ~1 MiB repetitive I64
    let mut map_offs = Vec::with_capacity(nc_rows + 1);
    map_offs.push(0u32);
    let mut m_keys = Vec::with_capacity(nc_rows * 2);
    let mut m_vals = Vec::with_capacity(nc_rows * 2);
    let rep_val = "REPEATED_MAP_VALUE_".repeat(16);
    for r in 0..nc_rows {
        m_keys.push("env");
        m_vals.push(rep_val.as_str());
        m_keys.push("region");
        m_vals.push(if r % 2 == 0 { "us-east-1" } else { "eu-west-1" });
        map_offs.push(m_keys.len() as u32);
    }
    let nested_batch = Batch::try_new_all(
        Arc::clone(&nested_comp_schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::List {
                        item_type: DataType::I64,
                        offsets: list_offs,
                        values: Box::new(Column::Flat(
                            FlatColumn::try_new(FlatData::I64(list_vals), None).unwrap(),
                        )),
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::MapUtf8(Box::new(
                        MapData::try_new(
                            map_offs,
                            Utf8Data::try_from_slices(m_keys).unwrap(),
                            Utf8Data::try_from_slices(m_vals).unwrap(),
                            None,
                        )
                        .unwrap(),
                    )),
                    None,
                )
                .unwrap(),
            ),
        ],
        nc_rows,
    )
    .unwrap();
    let mut nested_opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast);
    nested_opts.max_row_group_bytes = 256 * 1024;
    let (nested_stats, nested_scanned) = write_and_verify(
        &nested_comp_schema,
        std::slice::from_ref(&nested_batch),
        nested_opts,
    );
    assert_eq!(
        nested_stats.num_row_groups, 1,
        "compressible ~1.6 MiB List<I64> + MapUtf8 must stay 1 RG"
    );
    assert_eq!(
        collect_all_scalars(&nested_scanned, 0),
        collect_all_scalars(std::slice::from_ref(&nested_batch), 0)
    );
    assert_eq!(
        collect_all_scalars(&nested_scanned, 1),
        collect_all_scalars(std::slice::from_ref(&nested_batch), 1)
    );

    // 7. B1: Per-mini-block MAX_COL_MB_RAW_BYTES guard: single batch of 8,192 x 10 KiB
    //    repetitive Utf8 (80 MiB raw > 64 MiB MAX_BLOCK_RAW_BYTES, < 4 MiB stored) written
    //    with BOTH L0Fast and L1Compact (max_row_group_bytes = 64 MiB) must auto-split into
    //    >= 2 RGs and succeed through verify_full() + scan().
    let mb_guard_schema =
        Schema::try_new_arc(vec![Field::new("big_text", DataType::Utf8, false)]).unwrap();
    let mb_rows = 8_192usize;
    let pattern_10k = "B1_REPETITIVE_10KIB_BLOCK_GUARD_".repeat(320); // 10,240 B
    assert_eq!(pattern_10k.len(), 10 * 1024);
    let mb_batch = Batch::try_new_all(
        Arc::clone(&mb_guard_schema),
        vec![Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(std::iter::repeat_n(pattern_10k.as_str(), mb_rows))
                        .unwrap(),
                ),
                None,
            )
            .unwrap(),
        )],
        mb_rows,
    )
    .unwrap();
    for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
        let mut opts = WriteOptions::for_table(TableKind::Generic, profile);
        opts.row_group_size = 131_072;
        opts.mini_block_size = 8_192;
        opts.max_row_group_bytes = 64 * 1024 * 1024;
        let (stats, scanned) =
            write_and_verify(&mb_guard_schema, std::slice::from_ref(&mb_batch), opts);
        assert!(
            stats.num_row_groups >= 2,
            "8,192 x 10 KiB (80 MiB raw) single MB must auto-split into >= 2 RGs ({profile:?}), got {}",
            stats.num_row_groups
        );
        assert_eq!(scanned.iter().map(Batch::num_rows).sum::<usize>(), mb_rows);
        assert_eq!(
            scanned[0].column(0).unwrap().get_scalar(0).unwrap(),
            ScalarValue::Utf8(pattern_10k.clone())
        );
        let last_b = scanned.last().unwrap();
        assert_eq!(
            last_b
                .column(0)
                .unwrap()
                .get_scalar(last_b.num_rows() - 1)
                .unwrap(),
            ScalarValue::Utf8(pattern_10k.clone())
        );
    }

    // 8. B4: MAX_RG_RAW_BUFFER_BYTES (252 MiB) & Dict/Const/Rle (and List<Dict>/<Const>/<Rle>)
    //    materialized cap prevents OOM / MAX_BATCH_DECODED_BYTES overflow.
    let dict_vals = FlatColumn::try_new(
        FlatData::Utf8(Utf8Data::try_from_slices([pattern_10k.as_str()]).unwrap()),
        None,
    )
    .unwrap();
    let dict_col_8k =
        Column::Dict(DictColumn::try_new(dict_vals.clone(), vec![0u32; mb_rows], None).unwrap());
    assert_eq!(
        estimate_column_slice_materialized_bytes(&dict_col_8k, 0, mb_rows),
        estimate_column_bytes(&dict_col_8k),
        "Dict full-slice materialized estimate must match estimate_column_bytes"
    );
    assert!(
        estimate_column_slice_materialized_bytes(&dict_col_8k, 0, mb_rows) > MAX_COL_MB_RAW_BYTES
    );

    let const_col_8k =
        Column::try_new_const(ScalarValue::Utf8(pattern_10k.clone()), mb_rows).unwrap();
    assert_eq!(
        estimate_column_slice_materialized_bytes(&const_col_8k, 0, mb_rows),
        estimate_column_bytes(&const_col_8k)
    );

    let rle_col_4k = Column::Rle(RleColumn::try_new(dict_vals.clone(), vec![4_096u32]).unwrap());
    assert_eq!(
        estimate_column_slice_materialized_bytes(&rle_col_4k, 0, 4_096),
        estimate_column_bytes(&rle_col_4k)
    );
    assert!(
        estimate_column_slice_materialized_bytes(&rle_col_4k, 1_000, 1_000) >= 1_000 * 10 * 1024
    );

    // Cold-review List<Dict> / List<Const> / List<Rle> materialized recursion check:
    let list_offsets: Vec<u32> = (0..=4_096u32).collect();
    let list_of_dict = Column::Flat(
        FlatColumn::try_new(
            FlatData::List {
                item_type: DataType::Utf8,
                offsets: list_offsets,
                values: Box::new(Column::Dict(
                    DictColumn::try_new(dict_vals, vec![0u32; 4_096], None).unwrap(),
                )),
            },
            None,
        )
        .unwrap(),
    );
    assert!(
        estimate_column_slice_materialized_bytes(&list_of_dict, 0, 4_096) >= 4_096 * 10 * 1024,
        "List<Dict> materialized slice estimate must recurse into Dict materialized size"
    );

    // 8a. Single Dict column exceeding MAX_COL_MB_RAW_BYTES (80 MiB materialized) splits cleanly!
    let dict_batch =
        Batch::try_new_all(Arc::clone(&mb_guard_schema), vec![dict_col_8k], mb_rows).unwrap();
    let mut dict_opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast);
    dict_opts.max_row_group_bytes = 64 * 1024 * 1024;
    let (dict_stats, dict_scanned) = write_and_verify(
        &mb_guard_schema,
        std::slice::from_ref(&dict_batch),
        dict_opts,
    );
    assert!(
        dict_stats.num_row_groups >= 2,
        "Dict column with 80 MiB materialized raw must split via MAX_COL_MB_RAW_BYTES, got {}",
        dict_stats.num_row_groups
    );
    assert_eq!(
        dict_scanned.iter().map(Batch::num_rows).sum::<usize>(),
        mb_rows
    );

    // 8b. 7 Rle columns x 40 MiB each (<= 48 MiB per-MB cap, 280 MiB total > 252 MiB MAX_RG_RAW_BUFFER_BYTES)
    //     splits cleanly without hitting MAX_BATCH_DECODED_BYTES!
    let b4_cols = 7usize;
    let b4_rows = 4_096usize;
    let b4_schema = Schema::try_new_arc(
        (0..b4_cols)
            .map(|c| Field::new(format!("rle_{c}"), DataType::Utf8, false))
            .collect(),
    )
    .unwrap();
    let b4_batch =
        Batch::try_new_all(Arc::clone(&b4_schema), vec![rle_col_4k; b4_cols], b4_rows).unwrap();
    let mut b4_opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast);
    b4_opts.max_row_group_bytes = 64 * 1024 * 1024;
    let (b4_stats, b4_scanned) =
        write_and_verify(&b4_schema, std::slice::from_ref(&b4_batch), b4_opts);
    assert!(
        b4_stats.num_row_groups >= 2,
        "7 x 40 MiB Rle cols (280 MiB > 252 MiB MAX_RG_RAW_BUFFER_BYTES) must split into >= 2 RGs, got {}",
        b4_stats.num_row_groups
    );
    assert_eq!(
        b4_scanned.iter().map(Batch::num_rows).sum::<usize>(),
        b4_rows
    );
    assert_eq!(
        b4_scanned[0].column(0).unwrap().get_scalar(0).unwrap(),
        ScalarValue::Utf8(pattern_10k)
    );
}

#[test]
fn streaming_pack_builder_is_bit_exact_with_in_memory_and_owned_writer() {
    use crate::payload::pack::{ChunkPackReader, PackWriteOptions};

    let pack_id = [0x7Au8; 16];
    let opts = PackWriteOptions {
        target_frame_raw_bytes: 16 * 1024,
        enable_trigram_skip: true,
        emit_hash_table: true,
        ..PackWriteOptions::default()
    };

    // 1. Bit-exact parity between ChunkPackBuilder::new + finish and new_streaming + finish_into_inner.
    let mut mem_pb = ChunkPackBuilder::new(pack_id, opts);
    let mut stream_pb =
        ChunkPackBuilder::new_streaming(pack_id, opts, std::io::Cursor::new(Vec::new())).unwrap();
    mem_pb.enable_boundary_tracking();
    stream_pb.enable_boundary_tracking();
    assert_eq!(mem_pb.chunk_count(), 0);
    assert_eq!(stream_pb.chunk_count(), 0);

    for i in 0..40usize {
        let payload = format!("streaming-parity-chunk-{i:04}-{}", "abcd1234".repeat(160));
        let c1 = mem_pb.intern_chunk(payload.as_bytes()).unwrap();
        let c2 = stream_pb.intern_chunk(payload.as_bytes()).unwrap();
        assert_eq!(c1, c2);
        // Repeat every 5th chunk to exercise dedup hits.
        if i % 5 == 0 {
            let d1 = mem_pb.intern_chunk(payload.as_bytes()).unwrap();
            let d2 = stream_pb.intern_chunk(payload.as_bytes()).unwrap();
            assert_eq!(d1, d2);
            assert_eq!(d1, c1);
        }
        assert_eq!(mem_pb.chunk_count(), (i + 1) as u32);
        assert_eq!(stream_pb.chunk_count(), (i + 1) as u32);
    }
    // Exercise boundary chunk marking + bypass + blob.
    mem_pb.mark_boundary_chunks(&[0, 1]);
    stream_pb.mark_boundary_chunks(&[0, 1]);
    let b1 = mem_pb.append_raw_bypass(&[0x5Au8; 4096]).unwrap();
    let b2 = stream_pb.append_raw_bypass(&[0x5Au8; 4096]).unwrap();
    assert_eq!(b1, b2);
    let bl1 = mem_pb.append_blob(&[0x3Cu8; 40_000]).unwrap();
    let bl2 = stream_pb.append_blob(&[0x3Cu8; 40_000]).unwrap();
    assert_eq!(bl1, bl2);
    assert_eq!(mem_pb.chunk_count(), stream_pb.chunk_count());

    let mut mem_bytes = Vec::new();
    let mem_stats = mem_pb.finish(&mut mem_bytes).unwrap();
    let (stream_cursor, stream_stats) = stream_pb.finish_into_inner().unwrap();
    let stream_bytes = stream_cursor.into_inner();
    assert_eq!(stream_stats, mem_stats);
    assert_eq!(stream_bytes, mem_bytes);

    // Non-zero start_offset streaming pack writes cleanly at offset 37.
    let mut prefixed = std::io::Cursor::new(vec![0xEEu8; 37]);
    prefixed.set_position(37);
    let mut off_pb = ChunkPackBuilder::new_streaming(pack_id, opts, prefixed).unwrap();
    off_pb
        .intern_chunk(b"hello-prefixed-streaming-pack")
        .unwrap();
    let (off_cursor, off_stats) = off_pb.finish_into_inner().unwrap();
    let off_buf = off_cursor.into_inner();
    assert_eq!(&off_buf[..37], &[0xEEu8; 37]);
    assert_eq!(off_buf.len() as u64, 37 + off_stats.total_bytes);
    let pr = ChunkPackReader::open(Arc::new(SliceReader::new(off_buf[37..].to_vec()))).unwrap();
    pr.verify_full(pack_id).unwrap();

    // 2. StatamWriter::new_with_owned_pack auto-derives pack_id when options.pack_id is None.
    let schema = Schema::try_new_arc(vec![
        Field::new("start_time", DataType::TimestampNs { utc: true }, false),
        Field::new("input", DataType::Utf8, true),
    ])
    .unwrap();
    let payload_a = "OWNED_PACK_PAYLOAD_A_".repeat(250);
    let payload_b = "OWNED_PACK_PAYLOAD_B_".repeat(250);
    let big_batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: vec![10, 20],
                        utc: true,
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices([payload_a.as_str(), payload_b.as_str()])
                            .unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
        ],
        2,
    )
    .unwrap();

    let owned_pack_id = [0x99u8; 16];
    let w_opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
    assert!(w_opts.pack_id.is_none());
    let pb = ChunkPackBuilder::new_streaming(owned_pack_id, opts, std::io::Cursor::new(Vec::new()))
        .unwrap();
    let mut writer: StatamWriter<'static, std::io::Cursor<Vec<u8>>, std::io::Cursor<Vec<u8>>> =
        StatamWriter::new_with_owned_pack(
            std::io::Cursor::new(Vec::new()),
            Arc::clone(&schema),
            w_opts.clone(),
            Some(pb),
        )
        .unwrap();
    writer.write_batch(&big_batch).unwrap();
    let (statam_c, w_stats, owned_pb) = writer.finish_into_inner().unwrap();
    assert_eq!(w_stats.pack_ids, vec![owned_pack_id]);
    let owned_pb = owned_pb.unwrap();
    assert!(owned_pb.chunk_count() > 0);
    let (pack_c, p_stats) = owned_pb.finish_into_inner().unwrap();
    assert_eq!(p_stats.pack_id, owned_pack_id);
    assert!(p_stats.toc_stored_bytes > 0);

    let pr =
        Arc::new(ChunkPackReader::open(Arc::new(SliceReader::new(pack_c.into_inner()))).unwrap());
    let mut resolver = InMemoryPackResolver::new();
    resolver.insert(owned_pack_id, Arc::clone(&pr));
    let reader = StatamReader::open(
        Arc::new(SliceReader::new(statam_c.into_inner())),
        Some(Arc::new(resolver)),
    )
    .unwrap();
    reader
        .verify_full_with_pack(Some((owned_pack_id, &pr)), 2, TableKind::Spans)
        .unwrap();

    // 3. 0-chunk pack case (all payloads inline) -> owned_pb.chunk_count() == 0, pack_ids empty.
    let inline_batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: vec![10, 20],
                        utc: true,
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["short1", "short2"]).unwrap()),
                    None,
                )
                .unwrap(),
            ),
        ],
        2,
    )
    .unwrap();
    let zero_pb =
        ChunkPackBuilder::new_streaming([0xAAu8; 16], opts, std::io::Cursor::new(Vec::new()))
            .unwrap();
    let mut zero_writer = StatamWriter::new_with_owned_pack(
        std::io::Cursor::new(Vec::new()),
        Arc::clone(&schema),
        w_opts,
        Some(zero_pb),
    )
    .unwrap();
    zero_writer.write_batch(&inline_batch).unwrap();
    let (zero_statam_c, zero_stats, zero_owned_pb) = zero_writer.finish_into_inner().unwrap();
    assert!(zero_stats.pack_ids.is_empty());
    assert_eq!(zero_owned_pb.unwrap().chunk_count(), 0);
    let zero_reader =
        StatamReader::open(Arc::new(SliceReader::new(zero_statam_c.into_inner())), None).unwrap();
    zero_reader
        .verify_full_with_pack(None, 2, TableKind::Spans)
        .unwrap();
}
