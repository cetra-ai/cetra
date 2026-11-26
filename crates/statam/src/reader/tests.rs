use std::ops::Range;

use cetra_column::{
    BinaryData, EventsData, Field, Interval, LinksData, MapData, RleColumn, Utf8Data,
};

use super::eval::eval_dict_pred;
use super::*;
use crate::WriteProfile;
use crate::codec::ZoneValue;
use crate::format::{
    BLOCK_ENTRY_LEN, BLOCK_FLAG_CONST_EMPTY_COLLECTION, COL_FLAG_BLOOM_IN_HOT_TAIL,
    COLUMN_DIR_ENTRY_LEN, ChunkLoc, Codec, ColumnDirEntry, FLAG_HOT_TAIL_LZ4, LAZY_INDEX_ENTRY_LEN,
    LazyIndexEntry, LazyIndexKind, MAX_BATCH_DECODED_BYTES, POSTSCRIPT_LEN, Postscript,
    ROW_GROUP_ENTRY_LEN, TableKind,
};
use crate::io::{SimulatedS3Config, SimulatedS3Reader, SliceReader};
use crate::payload::decode_payload_refs_flat;
use crate::payload::pack::{ChunkPackBuilder, PackWriteOptions};
use crate::writer::{L0_PAYLOAD_DIR_MAGIC, StatamWriter, WriteOptions};

fn test_pack_opts() -> PackWriteOptions {
    PackWriteOptions {
        #[cfg(miri)]
        codec: Codec::Lz4,
        #[cfg(not(miri))]
        codec: Codec::Zstd,
        zstd_level: 3,
        target_frame_raw_bytes: 16 * 1024,
        enable_trigram_skip: true,
        compress_toc_lz4: true,
        write_threads: 1,
        ..PackWriteOptions::default()
    }
}

fn make_spans_schema() -> Arc<Schema> {
    Schema::try_new_arc(vec![
        Field::new("trace_id", DataType::FixedBytes16, false),
        Field::new("span_id", DataType::FixedBytes8, false),
        Field::new("start_time", DataType::TimestampNs { utc: true }, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("status_code", DataType::Utf8, false),
        Field::new("model", DataType::Utf8, true),
        Field::new("session_id", DataType::Utf8, true),
        Field::new("user_id", DataType::Utf8, true),
        Field::new("cost_usd", DataType::cost_decimal(), true),
        Field::new("latency_ms", DataType::F64, true),
        Field::new("is_error", DataType::Bool, false),
        Field::new("tags", DataType::List(Box::new(DataType::Utf8)), true),
        Field::new("attributes", DataType::MapUtf8, true),
        Field::new("events", DataType::Events, true),
        Field::new("links", DataType::Links, true),
        Field::new("input", DataType::Utf8, true),
        Field::new("output", DataType::Utf8, true),
    ])
    .unwrap()
}

fn make_spans_batch(schema: &Arc<Schema>, start_row: usize, count: usize) -> Batch {
    let mut trace_ids = Vec::with_capacity(count);
    let mut span_ids = Vec::with_capacity(count);
    let mut start_times = Vec::with_capacity(count);
    let mut names = Vec::with_capacity(count);
    let mut status_codes = Vec::with_capacity(count);
    let mut models = Vec::with_capacity(count);
    let mut session_ids = Vec::with_capacity(count);
    let mut user_ids = Vec::with_capacity(count);
    let mut costs = Vec::with_capacity(count);
    let mut latencies = Vec::with_capacity(count);
    let mut is_errors = Vec::with_capacity(count);

    let mut tag_offsets = vec![0u32];
    let mut tag_items = Vec::new();
    let mut map_offsets = vec![0u32];
    let mut map_keys = Vec::new();
    let mut map_vals = Vec::new();
    let mut map_val_valid = Vec::new();
    let mut ev_offsets = vec![0u32];
    let mut ev_times = Vec::new();
    let mut ev_names = Vec::new();
    let mut ev_attrs = Vec::new();
    let mut lk_offsets = vec![0u32];
    let mut lk_tids = Vec::new();
    let mut lk_sids = Vec::new();
    let mut lk_attrs = Vec::new();
    let mut inputs = Vec::with_capacity(count);
    let mut outputs = Vec::with_capacity(count);

    for i in 0..count {
        let r = start_row + i;
        let mut tid = [0u8; 16];
        tid[0..8].copy_from_slice(&((r / 4) as u64).to_be_bytes());
        tid[15] = 0xAA;
        trace_ids.push(tid);

        let mut sid = [0u8; 8];
        sid.copy_from_slice(&(r as u64).to_be_bytes());
        span_ids.push(sid);

        start_times.push(1_700_000_000_000_000_000i64 + (r as i64) * 1_000_000);
        names.push(if r.is_multiple_of(10) {
            "llm.completion".to_owned()
        } else if r.is_multiple_of(3) {
            "tool.execute".to_owned()
        } else {
            "agent.step".to_owned()
        });
        status_codes.push(if r.is_multiple_of(7) { "ERROR" } else { "OK" });
        models.push(if r.is_multiple_of(2) {
            "gpt-4o"
        } else {
            "claude-3-5-sonnet"
        });
        session_ids.push(format!("sess_{}", r / 8));
        user_ids.push(format!("user_{}", r % 5));
        costs.push((r as i128 + 1) * 25_000_000_000_000);
        latencies.push(if r.is_multiple_of(29) {
            f64::NAN
        } else if r.is_multiple_of(17) {
            -0.0
        } else {
            (r % 500) as f64 * 1.25
        });
        is_errors.push(r.is_multiple_of(7));

        tag_items.push("prod");
        if r.is_multiple_of(2) {
            tag_items.push("streaming");
        }
        tag_offsets.push(tag_items.len() as u32);

        // Shredded hot keys (> 50% presence) + rare key.
        map_keys.push("customer_id".to_owned());
        map_vals.push(format!("cust_{}", r % 12));
        map_val_valid.push(true);
        map_keys.push("env".to_owned());
        map_vals.push("production".to_owned());
        map_val_valid.push(true);
        if r.is_multiple_of(50) {
            map_keys.push(format!("rare_{r}"));
            map_vals.push("v".to_owned());
            map_val_valid.push(true);
        }
        map_offsets.push(map_keys.len() as u32);

        if r.is_multiple_of(3) {
            ev_times.push(1_700_000_000_000_000_000i64 + r as i64);
            ev_names.push("token");
            ev_attrs.push("{\"count\":1}");
        }
        ev_offsets.push(ev_times.len() as u32);

        if r.is_multiple_of(5) {
            lk_tids.push(tid);
            lk_sids.push(sid);
            lk_attrs.push("{\"rel\":\"parent\"}");
        }
        lk_offsets.push(lk_tids.len() as u32);

        if r.is_multiple_of(4) {
            inputs.push(format!("short prompt {r}"));
        } else {
            let p0 = format!(
                "{{\"role\":\"system\",\"content\":\"{}\"}}",
                "S".repeat(280)
            );
            let p1 = format!(
                "{{\"role\":\"user\",\"content\":\"row {r} {}\"}}",
                "U".repeat(300)
            );
            inputs.push(format!("[{p0},{p1}]"));
        }
        outputs.push(format!(
            "{{\"answer\":\"response for row {r} {}\"}}",
            "R".repeat(300)
        ));
    }

    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let sess_refs: Vec<&str> = session_ids.iter().map(String::as_str).collect();
    let user_refs: Vec<&str> = user_ids.iter().map(String::as_str).collect();
    let mk_refs: Vec<&str> = map_keys.iter().map(String::as_str).collect();
    let mv_refs: Vec<&str> = map_vals.iter().map(String::as_str).collect();
    let in_refs: Vec<&str> = inputs.iter().map(String::as_str).collect();
    let out_refs: Vec<&str> = outputs.iter().map(String::as_str).collect();

    let cols = vec![
        Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(trace_ids), None).unwrap()),
        Column::Flat(FlatColumn::try_new(FlatData::FixedBytes8(span_ids), None).unwrap()),
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
        Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(name_refs).unwrap()),
                None,
            )
            .unwrap(),
        ),
        Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(status_codes).unwrap()),
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
                FlatData::Utf8(Utf8Data::try_from_slices(sess_refs).unwrap()),
                None,
            )
            .unwrap(),
        ),
        Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(user_refs).unwrap()),
                None,
            )
            .unwrap(),
        ),
        Column::Flat(
            FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: costs,
                    precision: 38,
                    scale: 18,
                },
                None,
            )
            .unwrap(),
        ),
        Column::Flat(FlatColumn::try_new(FlatData::F64(latencies), None).unwrap()),
        Column::Flat(
            FlatColumn::try_new(FlatData::Bool(Bitmap::from_bools(&is_errors)), None).unwrap(),
        ),
        Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: tag_offsets,
                    values: Box::new(Column::Flat(
                        FlatColumn::try_new(
                            FlatData::Utf8(Utf8Data::try_from_slices(tag_items).unwrap()),
                            None,
                        )
                        .unwrap(),
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
                        map_offsets,
                        Utf8Data::try_from_slices(mk_refs).unwrap(),
                        Utf8Data::try_from_slices(mv_refs).unwrap(),
                        None,
                    )
                    .unwrap(),
                )),
                None,
            )
            .unwrap(),
        ),
        Column::Flat(
            FlatColumn::try_new(
                FlatData::Events(Box::new(
                    EventsData::try_new(
                        ev_offsets,
                        ev_times,
                        Utf8Data::try_from_slices(ev_names).unwrap(),
                        Utf8Data::try_from_slices(ev_attrs).unwrap(),
                        None,
                    )
                    .unwrap(),
                )),
                None,
            )
            .unwrap(),
        ),
        Column::Flat(
            FlatColumn::try_new(
                FlatData::Links(Box::new(
                    LinksData::try_new(
                        lk_offsets,
                        lk_tids,
                        lk_sids,
                        Utf8Data::try_from_slices(lk_attrs).unwrap(),
                        None,
                    )
                    .unwrap(),
                )),
                None,
            )
            .unwrap(),
        ),
        Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(in_refs).unwrap()),
                None,
            )
            .unwrap(),
        ),
        Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(out_refs).unwrap()),
                None,
            )
            .unwrap(),
        ),
    ];

    Batch::try_new_all(Arc::clone(schema), cols, count).unwrap()
}

fn concat_scanned_batches(schema: &Arc<Schema>, batches: &[Batch]) -> Batch {
    if batches.is_empty() {
        return Batch::empty(Arc::clone(schema));
    }
    let refs: Vec<&Batch> = batches.iter().collect();
    Batch::concat(Arc::clone(schema), &refs, None).unwrap()
}

#[test]
fn end_to_end_roundtrip_spans_traces_scores_l0_and_l1() {
    // 1. Spans (multi-batch, L0Fast & L1Compact with small RG/MB sizes to exercise multi-RG streaming).
    let spans_schema = make_spans_schema();
    let b0 = make_spans_batch(&spans_schema, 0, 140);
    let b1 = make_spans_batch(&spans_schema, 140, 110);
    let expected_spans = Batch::concat(Arc::clone(&spans_schema), &[&b0, &b1], None).unwrap();

    for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
        let mut opts = WriteOptions::for_table(TableKind::Spans, profile);
        opts.sort_cols = vec![2, 0];
        opts.row_group_size = 128;
        opts.mini_block_size = 64;

        let mut resolver = InMemoryPackResolver::new();
        let mut file_bytes = Vec::new();
        let stats = if profile == WriteProfile::L1Compact {
            let pack_id = [0x11u8; 16];
            opts.pack_id = Some(pack_id);
            let mut pb = ChunkPackBuilder::new(pack_id, test_pack_opts());
            let mut writer = StatamWriter::new(
                std::io::Cursor::new(&mut file_bytes),
                Arc::clone(&spans_schema),
                opts,
                Some(&mut pb),
            )
            .unwrap();
            writer.write_batch(&b0).unwrap();
            writer.write_batch(&b1).unwrap();
            let stats = writer.finish().unwrap();
            let mut pack_bytes = Vec::new();
            pb.finish(&mut pack_bytes).unwrap();
            resolver.insert_bytes(pack_id, pack_bytes).unwrap();
            stats
        } else {
            let mut writer = StatamWriter::new(
                std::io::Cursor::new(&mut file_bytes),
                Arc::clone(&spans_schema),
                opts,
                None,
            )
            .unwrap();
            writer.write_batch(&b0).unwrap();
            writer.write_batch(&b1).unwrap();
            writer.finish().unwrap()
        };

        assert_eq!(stats.row_count, 250);
        assert_eq!(stats.num_row_groups, 2);

        let reader = StatamReader::open(
            Arc::new(SliceReader::new(file_bytes)),
            Some(Arc::new(resolver)),
        )
        .unwrap();
        reader.verify_full().unwrap();

        let got = reader.scan(&ScanRequest::default()).unwrap();
        let actual = concat_scanned_batches(&spans_schema, &got);
        assert_eq!(actual.num_rows(), expected_spans.num_rows());
        for c in 0..spans_schema.len() {
            assert!(
                actual
                    .column(c)
                    .unwrap()
                    .semantic_eq(expected_spans.column(c).unwrap()),
                "spans roundtrip mismatch at col {c} ({}) for {profile:?}",
                spans_schema.fields()[c].name
            );
        }
    }

    // 2. Traces table roundtrip (L0Fast & L1Compact).
    let traces_schema = Schema::try_new_arc(vec![
        Field::new("trace_id", DataType::FixedBytes16, false),
        Field::new("session_id", DataType::Utf8, true),
        Field::new("user_id", DataType::Utf8, true),
        Field::new("start_time", DataType::TimestampNs { utc: true }, false),
        Field::new("duration_ns", DataType::I64, false),
        Field::new("total_cost", DataType::cost_decimal(), true),
        Field::new("metadata", DataType::MapUtf8, true),
    ])
    .unwrap();
    let n = 64;
    let tids: Vec<[u8; 16]> = (0..n).map(|i| [i as u8; 16]).collect();
    let sess: Vec<&str> = (0..n)
        .map(|i| if i % 2 == 0 { "s0" } else { "s1" })
        .collect();
    let users: Vec<&str> = (0..n)
        .map(|i| if i % 3 == 0 { "u0" } else { "u1" })
        .collect();
    let ts: Vec<i64> = (0..n).map(|i| 1_000_000 + i as i64 * 10).collect();
    let dur: Vec<i64> = (0..n).map(|i| 500 + i as i64).collect();
    let cost: Vec<i128> = (0..n).map(|i| 1000 + i as i128).collect();
    let trace_batch = Batch::try_new_all(
        Arc::clone(&traces_schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(tids), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(sess).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(users).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: ts,
                        utc: true,
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(FlatColumn::try_new(FlatData::I64(dur), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Decimal128 {
                        values: cost,
                        precision: 38,
                        scale: 18,
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::try_new_const(ScalarValue::MapUtf8(Vec::new()), n).unwrap(),
        ],
        n,
    )
    .unwrap();

    for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
        let opts = WriteOptions::for_table(TableKind::Traces, profile);
        let mut buf = Vec::new();
        let mut w = StatamWriter::new(
            std::io::Cursor::new(&mut buf),
            Arc::clone(&traces_schema),
            opts,
            None,
        )
        .unwrap();
        w.write_batch(&trace_batch).unwrap();
        w.finish().unwrap();
        let r = StatamReader::open(Arc::new(SliceReader::new(buf)), None).unwrap();
        r.verify_full().unwrap();
        let got = concat_scanned_batches(&traces_schema, &r.scan(&ScanRequest::default()).unwrap());
        for c in 0..traces_schema.len() {
            assert!(
                got.column(c)
                    .unwrap()
                    .semantic_eq(trace_batch.column(c).unwrap())
            );
        }
    }

    // 3. Scores table roundtrip (L0Fast & L1Compact).
    let scores_schema = Schema::try_new_arc(vec![
        Field::new("trace_id", DataType::FixedBytes16, false),
        Field::new("score_id", DataType::FixedBytes16, false),
        Field::new("timestamp", DataType::TimestampNs { utc: true }, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("value", DataType::F64, false),
        Field::new("date", DataType::Date, false),
        Field::new("window", DataType::Interval, true),
        Field::new("raw_bytes", DataType::Binary, true),
    ])
    .unwrap();
    let sc_tids: Vec<[u8; 16]> = (0..n).map(|i| [i as u8; 16]).collect();
    let sc_ids: Vec<[u8; 16]> = (0..n).map(|i| [(i + 100) as u8; 16]).collect();
    let sc_ts: Vec<i64> = (0..n).map(|i| 5_000_000 + i as i64).collect();
    let sc_names = vec!["faithfulness"; n];
    let sc_vals: Vec<f64> = (0..n).map(|i| (i as f64) * 0.01).collect();
    let sc_dates: Vec<i32> = vec![19_800; n];
    let sc_ivs: Vec<Interval> = vec![
        Interval {
            months: 0,
            days: 1,
            nanos: 100,
        };
        n
    ];
    let sc_bins: Vec<&[u8]> = vec![b"\x01\x02\x03"; n];
    let scores_batch = Batch::try_new_all(
        Arc::clone(&scores_schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(sc_tids), None).unwrap()),
            Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(sc_ids), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: sc_ts,
                        utc: true,
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(sc_names).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(FlatColumn::try_new(FlatData::F64(sc_vals), None).unwrap()),
            Column::Flat(FlatColumn::try_new(FlatData::Date(sc_dates), None).unwrap()),
            Column::Flat(FlatColumn::try_new(FlatData::Interval(sc_ivs), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Binary(BinaryData::try_from_slices(sc_bins).unwrap()),
                    None,
                )
                .unwrap(),
            ),
        ],
        n,
    )
    .unwrap();

    for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
        let opts = WriteOptions::for_table(TableKind::Scores, profile);
        let mut buf = Vec::new();
        let mut w = StatamWriter::new(
            std::io::Cursor::new(&mut buf),
            Arc::clone(&scores_schema),
            opts,
            None,
        )
        .unwrap();
        w.write_batch(&scores_batch).unwrap();
        w.finish().unwrap();
        let r = StatamReader::open(Arc::new(SliceReader::new(buf)), None).unwrap();
        r.verify_full().unwrap();
        let got = concat_scanned_batches(&scores_schema, &r.scan(&ScanRequest::default()).unwrap());
        for c in 0..scores_schema.len() {
            assert!(
                got.column(c)
                    .unwrap()
                    .semantic_eq(scores_batch.column(c).unwrap())
            );
        }
    }
}

#[test]
fn cold_open_is_1_get_and_point_lookup_is_le_2_gets_on_simulated_s3() {
    // Build a > 2 MiB L0Fast file across multiple RGs using incompressible ASCII payloads
    // so byte 0 and the Hot Tail lie in separate 1 MiB S3 ReadCache blocks!
    let schema = make_spans_schema();
    let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
    opts.sort_cols = vec![2, 0];
    opts.row_group_size = 512;
    opts.mini_block_size = 256;

    let mut base_b0 = make_spans_batch(&schema, 0, 1024);
    let mut base_b1 = make_spans_batch(&schema, 1024, 1024);

    let make_incompressible_col = |seed_base: u64, count: usize| -> Column {
        let mut rng = seed_base;
        let mut vals = Vec::with_capacity(count);
        for _ in 0..count {
            let mut s = String::with_capacity(1400);
            for _ in 0..1400 {
                rng = rng
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                s.push((b'!' + ((rng >> 56) as u8 % 90)) as char);
            }
            vals.push(s);
        }
        let refs: Vec<&str> = vals.iter().map(String::as_str).collect();
        Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(refs).unwrap()),
                None,
            )
            .unwrap(),
        )
    };
    let replace_input = |batch: Batch, seed: u64| -> Batch {
        let (sc, mut cols, raw_len, sel) = batch.into_parts();
        cols[15] = make_incompressible_col(seed, raw_len);
        Batch::try_new(sc, cols, raw_len, sel).unwrap()
    };
    base_b0 = replace_input(base_b0, 0x1111_2222_3333_4444);
    base_b1 = replace_input(base_b1, 0x5555_6666_7777_8888);

    let mut file_bytes = Vec::new();
    let mut writer = StatamWriter::new(
        std::io::Cursor::new(&mut file_bytes),
        Arc::clone(&schema),
        opts,
        None,
    )
    .unwrap();
    writer.write_batch(&base_b0).unwrap();
    writer.write_batch(&base_b1).unwrap();
    writer.finish().unwrap();
    assert!(
        file_bytes.len() > 2 * 1024 * 1024,
        "expected > 2 MiB file, got {}",
        file_bytes.len()
    );

    let s3 = Arc::new(SimulatedS3Reader::from_bytes(
        file_bytes,
        SimulatedS3Config::default(),
    ));

    // 1. Cold open must be strictly 1 GET!
    let reader = StatamReader::open(s3.clone(), None).unwrap();
    assert_eq!(s3.counters().gets, 1, "cold open must take exactly 1 GET");
    assert!(
        (reader.meta().col_dir[0].col_flags & COL_FLAG_BLOOM_IN_HOT_TAIL) != 0,
        "trace_id bloom must reside in Hot Tail"
    );

    // 2. Absent trace_id lookup must prune via Hot Bloom with 0 extra GETs!
    let absent_tid = [0xFFu8; 16];
    let absent_res = reader
        .scan(&ScanRequest {
            id_lookups: vec![IdLookup::eq("trace_id", absent_tid)],
            ..ScanRequest::default()
        })
        .unwrap();
    assert!(absent_res.is_empty());
    assert_eq!(
        s3.counters().gets,
        1,
        "absent trace_id must use 0 extra GETs"
    );

    // 3. Present trace_id point lookup (full SELECT * on L0Fast) -> total GETs <= 2!
    let target_row = 1200usize;
    let mut target_tid = [0u8; 16];
    target_tid[0..8].copy_from_slice(&((target_row / 4) as u64).to_be_bytes());
    target_tid[15] = 0xAA;

    let res = reader
        .scan(&ScanRequest {
            id_lookups: vec![IdLookup::eq("trace_id", target_tid)],
            coalesce_mode: CoalesceMode::Auto,
            ..ScanRequest::default()
        })
        .unwrap();
    let total_gets = s3.counters().gets;
    assert!(
        total_gets <= 2,
        "cold open + point lookup must be <= 2 GETs, got {total_gets}"
    );
    let out = concat_scanned_batches(&schema, &res);
    assert_eq!(out.num_rows(), 4);
}

#[test]
fn late_materialization_skips_wide_columns_on_empty_filter() {
    let schema = Schema::try_new_arc(vec![
        Field::new("trace_id", DataType::FixedBytes16, false),
        Field::new("start_time", DataType::TimestampNs { utc: true }, false),
        Field::new("status_code", DataType::Utf8, false),
        Field::new("attributes", DataType::MapUtf8, true),
        Field::new("input", DataType::Utf8, true),
    ])
    .unwrap();
    let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
    opts.payload_cols = vec!["input".to_owned()];
    opts.row_group_size = 4096;
    opts.mini_block_size = 2048;

    let n = 4096usize;
    let tids: Vec<[u8; 16]> = (0..n).map(|i| [(i & 0xFF) as u8; 16]).collect();
    let ts: Vec<i64> = (0..n).map(|i| 1_000_000 + i as i64).collect();
    let status: Vec<&str> = (0..n)
        .map(|i| if i % 2 == 0 { "OK" } else { "ERR" })
        .collect();
    let mut map_offsets = vec![0u32];
    let mut map_keys = Vec::new();
    let mut map_vals = Vec::new();
    for i in 0..n {
        map_keys.push("k");
        map_vals.push(if i % 2 == 0 { "a" } else { "b" });
        map_offsets.push((i + 1) as u32);
    }
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    let inputs: Vec<String> = (0..n)
        .map(|i| {
            let mut s = format!("payload_{i}_");
            for _ in 0..700 {
                rng = rng
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                s.push((b'!' + ((rng >> 56) as u8 % 90)) as char);
            }
            s
        })
        .collect();
    let in_refs: Vec<&str> = inputs.iter().map(String::as_str).collect();

    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(tids), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: ts,
                        utc: true,
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(status).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::MapUtf8(Box::new(
                        MapData::try_new(
                            map_offsets,
                            Utf8Data::try_from_slices(map_keys).unwrap(),
                            Utf8Data::try_from_slices(map_vals).unwrap(),
                            None,
                        )
                        .unwrap(),
                    )),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(in_refs).unwrap()),
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
    w.finish().unwrap();
    assert!(file_bytes.len() > 2 * 1024 * 1024);

    let s3 = Arc::new(SimulatedS3Reader::from_bytes(
        file_bytes,
        SimulatedS3Config::default(),
    ));
    let reader = StatamReader::open(s3.clone(), None).unwrap();
    let rg0 = &reader.meta().rg_dir[0];
    let core_end = rg0.core_offset + u64::from(rg0.core_len);
    assert!(core_end < 1024 * 1024);

    s3.reset_counters();
    // Phase 1 actually reads and evaluates `start_time == 1_000_000` (even row -> "OK") AND `status_code == "ERR"`,
    // so both mini-blocks pass zone maps on `start_time` (row 1_000_000 in mb0) / or `start_time == 1_000_000` passes mb0,
    // evaluates Phase 1 to 0 rows, and MUST NOT read WideRow `attributes` or ExternalPayload `input`!
    let res = reader
        .scan(&ScanRequest {
            projection: None,
            predicates: vec![
                Predicate::new(
                    "start_time",
                    ZonePred::Eq(ZoneValue::TimestampNs(1_000_000)),
                ),
                Predicate::new(
                    "status_code",
                    ZonePred::Eq(ZoneValue::Bytes(b"ERR".to_vec())),
                ),
            ],
            coalesce_mode: CoalesceMode::Scan,
            ..ScanRequest::default()
        })
        .unwrap();
    assert!(res.is_empty());
    assert!(
        s3.counters().bytes_requested <= u64::from(rg0.core_len) + 64,
        "late materialization must NOT fetch WideRow/L0Payload when Phase 1 yields 0 rows; requested {}",
        s3.counters().bytes_requested
    );

    // Verify `projection = Some(vec![])` emits exact active row count without reading WideRow/ExternalPayload!
    s3.reset_counters();
    let count_batches = reader
        .scan(&ScanRequest {
            projection: Some(vec![]),
            time_range: TimeRange::new(1_000_010, 1_000_059),
            coalesce_mode: CoalesceMode::Scan,
            ..ScanRequest::default()
        })
        .unwrap();
    let total_active: usize = count_batches.iter().map(Batch::num_rows).sum();
    assert_eq!(total_active, 50);
    assert!(
        s3.counters().bytes_requested <= u64::from(rg0.core_len) + 128,
        "projection=Some(vec![]) must only read Core filter blocks, requested {} bytes",
        s3.counters().bytes_requested
    );
}

#[test]
fn shredded_map_key_query_reads_only_subcolumn_and_skips_residual() {
    let schema = Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("attributes", DataType::MapUtf8, true),
    ])
    .unwrap();

    let n = 128usize;
    let ids: Vec<i64> = (0..n as i64).collect();
    let mut map_offsets = vec![0u32];
    let mut map_keys = Vec::new();
    let mut map_vals = Vec::new();
    let big_rare = "R".repeat(4096);
    for i in 0..n {
        map_keys.push("customer_id".to_owned());
        map_vals.push(format!("cust_{}", i % 8));
        map_keys.push(format!("rare_key_{i}"));
        map_vals.push(big_rare.clone());
        map_offsets.push(map_keys.len() as u32);
    }
    let mk_refs: Vec<&str> = map_keys.iter().map(String::as_str).collect();
    let mv_refs: Vec<&str> = map_vals.iter().map(String::as_str).collect();

    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::MapUtf8(Box::new(
                        MapData::try_new(
                            map_offsets,
                            Utf8Data::try_from_slices(mk_refs).unwrap(),
                            Utf8Data::try_from_slices(mv_refs).unwrap(),
                            None,
                        )
                        .unwrap(),
                    )),
                    None,
                )
                .unwrap(),
            ),
        ],
        n,
    )
    .unwrap();

    let opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
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

    let s3 = Arc::new(SimulatedS3Reader::from_bytes(
        file_bytes,
        SimulatedS3Config::default(),
    ));
    let reader = StatamReader::open(s3.clone(), None).unwrap();
    let sm = &reader.meta().schema_meta.shredded_maps[0];
    let cust_entry = sm.keys.iter().find(|k| k.key == "customer_id").unwrap();
    assert_eq!(cust_entry.residual_count, 0);

    let residual_blk = reader.meta().rg_blocks(1, 0)[0];
    let sub_blk = reader.meta().rg_blocks(cust_entry.sub_col_idx as usize, 0)[0];
    assert!(residual_blk.stored_len > 0);
    assert!(sub_blk.offset < reader.meta().rg_dir[0].wide_offset);

    s3.reset_counters();
    let res = reader
        .scan(&ScanRequest {
            projection: Some(vec![0, 1]),
            map_keys: vec![("attributes".to_owned(), "customer_id".to_owned())],
            predicates: vec![Predicate::map_key(
                "attributes",
                "customer_id",
                ZonePred::Eq(ZoneValue::Bytes(b"cust_3".to_vec())),
            )],
            coalesce_mode: CoalesceMode::Scan,
            ..ScanRequest::default()
        })
        .unwrap();

    let expected_max_bytes = u64::from(reader.meta().rg_dir[0].core_len) + 64;
    assert!(
        s3.counters().bytes_requested <= expected_max_bytes,
        "expected <= {expected_max_bytes} requested (skipping residual {} B), got {}",
        residual_blk.stored_len,
        s3.counters().bytes_requested
    );

    let out = concat_scanned_batches(&schema, &res);
    assert_eq!(out.num_rows(), n / 8);
    let FlatData::MapUtf8(got_map) = out
        .column(1)
        .unwrap()
        .materialize(None)
        .unwrap()
        .data()
        .clone()
    else {
        panic!("expected MapUtf8");
    };
    for r in 0..out.num_rows() {
        let entries = got_map.get(r).unwrap();
        assert_eq!(
            entries,
            vec![("customer_id".to_owned(), Some("cust_3".to_owned()))]
        );
    }
}

#[test]
fn substring_search_e2e_inline_pack_boundary_and_spans_name() {
    let schema = Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("input", DataType::Utf8, true),
    ])
    .unwrap();

    let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
    opts.narrow_trigram_cols = vec!["name".to_owned()];
    let pack_id = [0x33u8; 16];
    opts.pack_id = Some(pack_id);

    let n = 2048usize;
    let ids: Vec<i64> = (0..n as i64).collect();
    let mut names = Vec::with_capacity(n);
    let mut inputs = Vec::with_capacity(n);

    // Encode each row's unique token in a base-4 alphabet (`0..3`) so the
    // column stays high-NDV (`>50%` distinct, avoiding Dict) and exceeds the
    // 2560 B compressed size floor for NarrowTrigram, while keeping total
    // unique trigrams small enough that trigram bloom density stays <= 0.50.
    let base4 = |mut v: usize| -> String {
        let mut s = String::with_capacity(8);
        for _ in 0..8 {
            s.push((b'0' + (v & 3) as u8) as char);
            v >>= 2;
        }
        s
    };

    for i in 0..n {
        names.push(if i == 7 {
            format!("agent.SPECIAL_SPAN_TOKEN_{i}_padding_abcdef1234567890")
        } else {
            let a = base4(i.wrapping_mul(2654435761) >> 16);
            let b = base4(i.wrapping_mul(1597334677) >> 16);
            let c = base4(i);
            format!("worker.op_{c}_svc_{a}_reg_{b}_xyz")
        });

        if i == 2 {
            inputs.push("inline TINY_NEEDLE_MATCH ok".to_owned());
        } else if i == 11 {
            let left = format!("\"{}BOUNDARY_\"", "a".repeat(270));
            let right = format!("\"SPLIT_TOKEN{}\"", "b".repeat(270));
            inputs.push(format!("[{left},{right}]"));
        } else {
            inputs.push(format!("[{{\"m\":\"{}noise_{i}\"}}]", "n".repeat(300)));
        }
    }

    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let in_refs: Vec<&str> = inputs.iter().map(String::as_str).collect();
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(name_refs).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(in_refs).unwrap()),
                    None,
                )
                .unwrap(),
            ),
        ],
        n,
    )
    .unwrap();

    let mut pb = ChunkPackBuilder::new(pack_id, test_pack_opts());
    let mut file_bytes = Vec::new();
    let mut writer = StatamWriter::new(
        std::io::Cursor::new(&mut file_bytes),
        Arc::clone(&schema),
        opts,
        Some(&mut pb),
    )
    .unwrap();
    writer.write_batch(&batch).unwrap();
    writer.finish().unwrap();
    let mut pack_bytes = Vec::new();
    pb.finish(&mut pack_bytes).unwrap();

    let mut resolver = InMemoryPackResolver::new();
    resolver.insert_bytes(pack_id, pack_bytes).unwrap();
    let reader = StatamReader::open(
        Arc::new(SliceReader::new(file_bytes)),
        Some(Arc::new(resolver)),
    )
    .unwrap();
    assert!(
        (reader.meta().col_dir[1].col_flags & COL_FLAG_HAS_TRIGRAM_INDEX) != 0,
        "high-NDV spans.name must receive NarrowTrigram index"
    );

    // 1. Inline match
    let res_inline = reader
        .scan(&ScanRequest {
            substrings: vec![SubstringQuery::like("input", "TINY_NEEDLE_MATCH")],
            ..ScanRequest::default()
        })
        .unwrap();
    let out_inline = concat_scanned_batches(&schema, &res_inline);
    assert_eq!(out_inline.num_rows(), 1);
    assert_eq!(
        out_inline.column(0).unwrap().get_scalar(0).unwrap(),
        ScalarValue::I64(2)
    );

    // 2. Cross-chunk boundary match straddling structural JSON comma boundary
    let res_boundary = reader
        .scan(&ScanRequest {
            substrings: vec![SubstringQuery::ilike("input", "boundary_\",\"split_token")],
            ..ScanRequest::default()
        })
        .unwrap();
    let out_boundary = concat_scanned_batches(&schema, &res_boundary);
    assert_eq!(out_boundary.num_rows(), 1);
    assert_eq!(
        out_boundary.column(0).unwrap().get_scalar(0).unwrap(),
        ScalarValue::I64(11)
    );

    // 3. Narrow `spans.name` trigram-indexed substring search
    let res_name = reader
        .scan(&ScanRequest {
            substrings: vec![SubstringQuery::ilike("name", "special_span_token_7")],
            ..ScanRequest::default()
        })
        .unwrap();
    let out_name = concat_scanned_batches(&schema, &res_name);
    assert_eq!(out_name.num_rows(), 1);
    assert_eq!(
        out_name.column(0).unwrap().get_scalar(0).unwrap(),
        ScalarValue::I64(7)
    );
}

#[test]
fn empty_file_desc_limit_and_column_encoding_passthrough() {
    let schema = Schema::try_new_arc(vec![
        Field::new("ts", DataType::TimestampNs { utc: true }, false),
        Field::new("const_col", DataType::Utf8, false),
        Field::new("rle_col", DataType::I64, false),
        Field::new("dict_col", DataType::Utf8, false),
    ])
    .unwrap();

    // 1. Empty 0-row file roundtrip.
    let mut empty_bytes = Vec::new();
    let w_empty = StatamWriter::new(
        std::io::Cursor::new(&mut empty_bytes),
        Arc::clone(&schema),
        WriteOptions::default(),
        None,
    )
    .unwrap();
    let stats_empty = w_empty.finish().unwrap();
    assert_eq!(stats_empty.row_count, 0);
    let r_empty = StatamReader::open(Arc::new(SliceReader::new(empty_bytes)), None).unwrap();
    r_empty.verify_full().unwrap();
    assert!(r_empty.scan(&ScanRequest::default()).unwrap().is_empty());

    // 2. Multi-RG DESC + LIMIT + Const/Rle/Dict pass-through.
    let mut opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L1Compact);
    opts.sort_cols = vec![0];
    opts.row_group_size = 32;
    opts.mini_block_size = 16;

    let n = 64usize;
    let ts: Vec<i64> = (0..n as i64).map(|i| 10_000 + i).collect();
    let rle = RleColumn::try_new(
        FlatColumn::try_new(FlatData::I64(vec![100, 200]), None).unwrap(),
        vec![32, 64],
    )
    .unwrap();
    let dict_vals: Vec<&str> = (0..n)
        .map(|i| match i % 3 {
            0 => "alpha",
            1 => "beta",
            _ => "gamma",
        })
        .collect();

    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: ts,
                        utc: true,
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::try_new_const(ScalarValue::Utf8("constant_tag".to_owned()), n).unwrap(),
            Column::Rle(rle),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(dict_vals).unwrap()),
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
    w.finish().unwrap();

    let reader = StatamReader::open(Arc::new(SliceReader::new(file_bytes)), None).unwrap();
    let batches = reader
        .scan(&ScanRequest {
            sort_desc_hint: true,
            limit: Some(5),
            ..ScanRequest::default()
        })
        .unwrap();
    let total: usize = batches.iter().map(Batch::num_rows).sum();
    assert_eq!(total, 5);
    assert!(matches!(
        batches[0].column(1).unwrap(),
        Column::Const { .. }
    ));
    assert!(matches!(
        batches[0].column(2).unwrap(),
        Column::Const { .. } | Column::Rle(_)
    ));
    assert!(matches!(batches[0].column(3).unwrap(), Column::Dict(_)));
    let got_desc = concat_scanned_batches(&schema, &batches);
    let got_ts: Vec<i64> = (0..5)
        .map(
            |r| match got_desc.column(0).unwrap().get_scalar(r).unwrap() {
                ScalarValue::TimestampNs { value, .. } => value,
                other => panic!("unexpected ts {other:?}"),
            },
        )
        .collect();
    assert_eq!(got_ts, vec![10_059, 10_060, 10_061, 10_062, 10_063]);
}

#[test]
fn desc_limit_returns_exact_newest_timestamps_on_20k_sorted_file() {
    let schema = Schema::try_new_arc(vec![
        Field::new("ts", DataType::TimestampNs { utc: true }, false),
        Field::new("flag", DataType::I64, false),
    ])
    .unwrap();

    let mut opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L1Compact);
    opts.sort_cols = vec![0];
    opts.row_group_size = 16_384;

    let n = 20_000usize;
    let ts: Vec<i64> = (0..n as i64).map(|i| 1_000_000 + i).collect();
    // Sparse flag = 1 at 19_999, 19_990, 16_383, 16_380, 15_000, 100 (spans multiple MBs & RGs)
    let flags: Vec<i64> = (0..n)
        .map(|i| {
            if matches!(i, 100 | 15_000 | 16_380 | 16_383 | 19_990 | 19_999) {
                1
            } else {
                0
            }
        })
        .collect();
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: ts,
                        utc: true,
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(FlatColumn::try_new(FlatData::I64(flags), None).unwrap()),
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
    w.finish().unwrap();

    let s3 = Arc::new(SimulatedS3Reader::from_bytes(
        file_bytes,
        SimulatedS3Config::default(),
    ));
    let reader = StatamReader::open(s3, None).unwrap();
    assert_eq!(reader.meta().num_row_groups(), 2);

    // 1. Unfiltered TOP 5 DESC
    let res = reader
        .scan(&ScanRequest {
            sort_desc_hint: true,
            limit: Some(5),
            ..ScanRequest::default()
        })
        .unwrap();
    let out = concat_scanned_batches(&schema, &res);
    assert_eq!(out.num_rows(), 5);
    let mut got_ts: Vec<i64> = (0..5)
        .map(|r| match out.column(0).unwrap().get_scalar(r).unwrap() {
            ScalarValue::TimestampNs { value, .. } => value,
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    got_ts.sort_unstable();
    assert_eq!(
        got_ts,
        vec![1_019_995, 1_019_996, 1_019_997, 1_019_998, 1_019_999]
    );

    // 2. Filtered TOP 5 DESC spanning multiple mini-blocks & both RGs
    let res_filtered = reader
        .scan(&ScanRequest {
            predicates: vec![Predicate::new("flag", ZonePred::Eq(ZoneValue::I64(1)))],
            sort_desc_hint: true,
            limit: Some(5),
            ..ScanRequest::default()
        })
        .unwrap();
    let out_f = concat_scanned_batches(&schema, &res_filtered);
    assert_eq!(out_f.num_rows(), 5);
    let mut got_f: Vec<i64> = (0..5)
        .map(|r| match out_f.column(0).unwrap().get_scalar(r).unwrap() {
            ScalarValue::TimestampNs { value, .. } => value,
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    got_f.sort_unstable();
    assert_eq!(
        got_f,
        vec![1_015_000, 1_016_380, 1_016_383, 1_019_990, 1_019_999]
    );
}

#[test]
fn shredded_map_filter_with_full_map_projection_preserves_rare_residual_keys() {
    let schema = Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("attributes", DataType::MapUtf8, true),
    ])
    .unwrap();

    let n = 128usize;
    let ids: Vec<i64> = (0..n as i64).collect();
    let mut map_offsets = vec![0u32];
    let mut map_keys = Vec::new();
    let mut map_vals = Vec::new();
    for i in 0..n {
        map_keys.push("env");
        map_vals.push(if i % 2 == 0 { "prod" } else { "staging" });
        if i == 10 {
            // Rare residual key (< 5% presence -> stays in residual!).
            map_keys.push("rare_tag");
            map_vals.push("vip_payload");
        }
        map_offsets.push(map_keys.len() as u32);
    }

    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::MapUtf8(Box::new(
                        MapData::try_new(
                            map_offsets,
                            Utf8Data::try_from_slices(map_keys).unwrap(),
                            Utf8Data::try_from_slices(map_vals).unwrap(),
                            None,
                        )
                        .unwrap(),
                    )),
                    None,
                )
                .unwrap(),
            ),
        ],
        n,
    )
    .unwrap();

    let opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
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
    let sm = &reader.meta().schema_meta.shredded_maps[0];
    let env_entry = sm.keys.iter().find(|k| k.key == "env").unwrap();
    assert_eq!(env_entry.residual_count, 0);

    // Filter on shredded key `"env" == "prod"` (residual_count == 0) WHILE projecting
    // the FULL `"attributes"` map (`map_keys: vec![]`) -> `"rare_tag"` MUST be preserved!
    let res = reader
        .scan(&ScanRequest {
            projection: Some(vec![0, 1]),
            map_keys: Vec::new(),
            predicates: vec![Predicate::map_key(
                "attributes",
                "env",
                ZonePred::Eq(ZoneValue::Bytes(b"prod".to_vec())),
            )],
            coalesce_mode: CoalesceMode::Scan,
            ..ScanRequest::default()
        })
        .unwrap();
    let out = concat_scanned_batches(&schema, &res);
    assert_eq!(out.num_rows(), n / 2);

    let FlatData::MapUtf8(got_map) = out
        .column(1)
        .unwrap()
        .materialize(None)
        .unwrap()
        .data()
        .clone()
    else {
        panic!("expected MapUtf8");
    };
    // Row with id=10 is at output row 5 (even rows 0, 2, 4, 6, 8, 10).
    let row10_entries = got_map.get(5).unwrap();
    assert!(
        row10_entries.contains(&("env".to_owned(), Some("prod".to_owned()))),
        "missing shredded key env"
    );
    assert!(
        row10_entries.contains(&("rare_tag".to_owned(), Some("vip_payload".to_owned()))),
        "rare residual key rare_tag was dropped during full map projection!"
    );
}

#[test]
fn f64_total_order_nan_semantics_on_flat_and_const_columns() {
    let schema = Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("flat_f", DataType::F64, true),
        Field::new("const_nan", DataType::F64, false),
        Field::new("const_neg_zero", DataType::F64, false),
    ])
    .unwrap();

    // Row 0: -0.0, Row 1: 0.5, Row 2: 2.0, Row 3: NaN, Row 4: +inf, Row 5: NULL
    let n = 6usize;
    let ids: Vec<i64> = (0..n as i64).collect();
    let flat_vals = vec![-0.0, 0.5, 2.0, f64::NAN, f64::INFINITY, 0.0];
    let flat_nulls = Bitmap::from_bools(&[true, true, true, true, true, false]);
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
            Column::Flat(FlatColumn::try_new(FlatData::F64(flat_vals), Some(flat_nulls)).unwrap()),
            Column::try_new_const(ScalarValue::F64(f64::NAN), n).unwrap(),
            Column::try_new_const(ScalarValue::F64(-0.0), n).unwrap(),
        ],
        n,
    )
    .unwrap();

    for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
        let opts = WriteOptions::for_table(TableKind::Generic, profile);
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

        let proj_schema = Arc::new(schema.project(&[0]).unwrap());
        let scan_ids = |preds: Vec<Predicate>| -> Vec<i64> {
            let res = reader
                .scan(&ScanRequest {
                    projection: Some(vec![0]),
                    predicates: preds,
                    ..ScanRequest::default()
                })
                .unwrap();
            let out = concat_scanned_batches(&proj_schema, &res);
            (0..out.num_rows())
                .map(|r| match out.column(0).unwrap().get_scalar(r).unwrap() {
                    ScalarValue::I64(v) => v,
                    other => panic!("unexpected {other:?}"),
                })
                .collect()
        };

        // Flat F64 total-order checks (NULL row 5 never matches value predicates)
        assert_eq!(
            scan_ids(vec![Predicate::new(
                "flat_f",
                ZonePred::Eq(ZoneValue::F64(f64::NAN))
            )]),
            vec![3]
        );
        assert_eq!(
            scan_ids(vec![Predicate::new(
                "flat_f",
                ZonePred::Ne(ZoneValue::F64(f64::NAN))
            )]),
            vec![0, 1, 2, 4]
        );
        assert_eq!(
            scan_ids(vec![Predicate::new(
                "flat_f",
                ZonePred::Gt(ZoneValue::F64(1.0))
            )]),
            vec![2, 3, 4]
        );
        assert_eq!(
            scan_ids(vec![Predicate::new(
                "flat_f",
                ZonePred::Between(ZoneValue::F64(0.0), ZoneValue::F64(f64::NAN))
            )]),
            vec![0, 1, 2, 3, 4]
        );
        assert_eq!(
            scan_ids(vec![Predicate::new(
                "flat_f",
                ZonePred::In(vec![ZoneValue::F64(f64::NAN)])
            )]),
            vec![3]
        );
        assert_eq!(
            scan_ids(vec![Predicate::new(
                "flat_f",
                ZonePred::Eq(ZoneValue::F64(0.0))
            )]),
            vec![0]
        );

        // Const F64(NaN) total-order checks
        assert_eq!(
            scan_ids(vec![Predicate::new(
                "const_nan",
                ZonePred::Eq(ZoneValue::F64(f64::NAN))
            )]),
            vec![0, 1, 2, 3, 4, 5]
        );
        assert!(
            scan_ids(vec![Predicate::new(
                "const_nan",
                ZonePred::Ne(ZoneValue::F64(f64::NAN))
            )])
            .is_empty()
        );
        assert_eq!(
            scan_ids(vec![Predicate::new(
                "const_nan",
                ZonePred::Gt(ZoneValue::F64(1.0))
            )]),
            vec![0, 1, 2, 3, 4, 5]
        );
        assert_eq!(
            scan_ids(vec![Predicate::new(
                "const_nan",
                ZonePred::Between(ZoneValue::F64(0.0), ZoneValue::F64(f64::NAN))
            )]),
            vec![0, 1, 2, 3, 4, 5]
        );
        assert_eq!(
            scan_ids(vec![Predicate::new(
                "const_nan",
                ZonePred::In(vec![ZoneValue::F64(f64::NAN)])
            )]),
            vec![0, 1, 2, 3, 4, 5]
        );
        assert_eq!(
            scan_ids(vec![Predicate::new(
                "const_neg_zero",
                ZonePred::Eq(ZoneValue::F64(0.0))
            )]),
            vec![0, 1, 2, 3, 4, 5]
        );
    }
}

#[test]
fn read_from_rejects_structural_corruption_and_short_reads() {
    let schema = Schema::try_new_arc(vec![
        Field::new("a", DataType::I64, false),
        Field::new("b", DataType::I64, false),
    ])
    .unwrap();
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1, 2, 3, 4]), None).unwrap()),
            Column::Flat(FlatColumn::try_new(FlatData::I64(vec![5, 6, 7, 8]), None).unwrap()),
        ],
        4,
    )
    .unwrap();
    let mut opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast);
    opts.compress_hot_tail_lz4 = false;
    let mut valid_bytes = Vec::new();
    let mut w = StatamWriter::new(
        std::io::Cursor::new(&mut valid_bytes),
        Arc::clone(&schema),
        opts,
        None,
    )
    .unwrap();
    w.write_batch(&batch).unwrap();
    w.finish().unwrap();

    let rebuild_uncompressed_tail = |mut raw_tail: Vec<u8>, mut ps: Postscript| -> Vec<u8> {
        let mut out = valid_bytes[..valid_bytes.len() - ps.hot_tail_len as usize].to_vec();
        ps.flags &= !FLAG_HOT_TAIL_LZ4;
        ps.hot_tail_raw_len = raw_tail.len() as u32;
        ps.hot_tail_len = (raw_tail.len() + POSTSCRIPT_LEN) as u32;
        ps.hot_tail_crc32c = crc32c::crc32c(&raw_tail);
        out.append(&mut raw_tail);
        out.extend_from_slice(&ps.encode());
        out
    };

    let meta = StatamFileMeta::read_from(&SliceReader::new(valid_bytes.clone()), None).unwrap();
    let ps = meta.postscript;
    let raw_body_start = valid_bytes.len() - ps.hot_tail_len as usize;
    let raw_body = valid_bytes[raw_body_start..valid_bytes.len() - POSTSCRIPT_LEN].to_vec();

    // 1. Corrupt col_dir[0].logical_col_id != 0
    {
        let mut body = raw_body.clone();
        let off = ps.col_dir_off as usize;
        let mut cd = ColumnDirEntry::decode(&body[off..off + COLUMN_DIR_ENTRY_LEN]).unwrap();
        cd.logical_col_id = 1;
        body[off..off + COLUMN_DIR_ENTRY_LEN].copy_from_slice(&cd.encode());
        let mutated = rebuild_uncompressed_tail(body, ps);
        assert!(StatamFileMeta::read_from(&SliceReader::new(mutated), None).is_err());
    }

    // 2. Corrupt rg.wide_offset != rg.core_offset + rg.core_len
    {
        let mut body = raw_body.clone();
        let off = ps.rg_dir_off as usize;
        let mut rg = RowGroupEntry::decode(&body[off..off + ROW_GROUP_ENTRY_LEN]).unwrap();
        rg.wide_offset += 1;
        rg.wide_len = rg.wide_len.saturating_sub(1);
        body[off..off + ROW_GROUP_ENTRY_LEN].copy_from_slice(&rg.encode());
        let mutated = rebuild_uncompressed_tail(body, ps);
        assert!(StatamFileMeta::read_from(&SliceReader::new(mutated), None).is_err());
    }

    // 3. Corrupt block layout mismatch across physical columns in RG 0
    {
        let mut body = raw_body;
        let b1_off = ps.block_dir_off as usize + BLOCK_ENTRY_LEN;
        let mut blk1 = BlockEntry::decode(&body[b1_off..b1_off + BLOCK_ENTRY_LEN]).unwrap();
        blk1.row_count = 3;
        body[b1_off..b1_off + BLOCK_ENTRY_LEN].copy_from_slice(&blk1.encode());
        let mutated = rebuild_uncompressed_tail(body, ps);
        assert!(StatamFileMeta::read_from(&SliceReader::new(mutated), None).is_err());
    }

    // 4. Short-reading RangeReader must fail cleanly
    struct ShortReader(Vec<u8>);
    impl RangeReader for ShortReader {
        fn file_len(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError> {
            let s = range.start as usize;
            let e = (range.end as usize).saturating_sub(1).max(s);
            Ok(Bytes::copy_from_slice(&self.0[s..e]))
        }
    }
    assert!(StatamFileMeta::read_from(&ShortReader(valid_bytes), None).is_err());
}

#[test]
fn masked_payload_substring_and_coalesced_stpk_read_chunks() {
    let schema = Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("tag", DataType::Utf8, false),
        Field::new("input", DataType::Utf8, true),
        Field::new("output", DataType::Utf8, true),
    ])
    .unwrap();

    let pack_id = [0x77u8; 16];
    let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
    opts.payload_cols = vec!["input".to_owned(), "output".to_owned()];
    opts.pack_id = Some(pack_id);
    opts.mini_block_size = 4;

    let n = 8usize;
    let ids: Vec<i64> = (0..n as i64).collect();
    let tags: Vec<&str> = (0..n)
        .map(|i| if i == 5 { "keep" } else { "skip" })
        .collect();
    let inputs: Vec<String> = (0..n)
        .map(|i| {
            format!(
                "[{{\"role\":\"user\",\"content\":\"IN_{i}_{}\"}}]",
                "x".repeat(320)
            )
        })
        .collect();
    let outputs: Vec<String> = (0..n)
        .map(|i| {
            format!(
                "[{{\"role\":\"assistant\",\"content\":\"OUT_{i}_{}\"}}]",
                "y".repeat(320)
            )
        })
        .collect();
    let in_refs: Vec<&str> = inputs.iter().map(String::as_str).collect();
    let out_refs: Vec<&str> = outputs.iter().map(String::as_str).collect();

    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(tags).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(in_refs).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(out_refs).unwrap()),
                    None,
                )
                .unwrap(),
            ),
        ],
        n,
    )
    .unwrap();

    let mut pb = ChunkPackBuilder::new(pack_id, test_pack_opts());
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

    let pack_s3 = Arc::new(SimulatedS3Reader::from_bytes(
        pack_bytes,
        SimulatedS3Config::default(),
    ));
    let pack_reader = Arc::new(ChunkPackReader::open(pack_s3.clone()).unwrap());
    let mut resolver = InMemoryPackResolver::new();
    resolver.insert(pack_id, pack_reader);

    let reader = StatamReader::open(
        Arc::new(SliceReader::new(file_bytes)),
        Some(Arc::new(resolver)),
    )
    .unwrap();

    // 1. When Phase 1 narrow predicate eliminates all rows (`tag == "missing"`),
    // payload substring search MUST NOT read the `.stpk` pack!
    pack_s3.reset_counters();
    let empty = reader
        .scan(&ScanRequest {
            predicates: vec![Predicate::new(
                "tag",
                ZonePred::Eq(ZoneValue::Bytes(b"missing".to_vec())),
            )],
            substrings: vec![SubstringQuery::like("input", "IN_5")],
            ..ScanRequest::default()
        })
        .unwrap();
    assert!(empty.is_empty());
    assert_eq!(pack_s3.counters().gets, 0);

    // 2. Projecting BOTH `input` and `output` across both mini-blocks reads `.stpk`
    // frames in a single coalesced wave (`gets == 1` when frames fit in 1 MiB).
    pack_s3.reset_counters();
    let res = reader
        .scan(&ScanRequest {
            projection: Some(vec![0, 2, 3]),
            ..ScanRequest::default()
        })
        .unwrap();
    let proj_schema = Arc::new(schema.project(&[0, 2, 3]).unwrap());
    let out = concat_scanned_batches(&proj_schema, &res);
    assert_eq!(out.num_rows(), n);
    assert!(pack_s3.counters().gets <= 1);
}

#[test]
fn corrupt_bf02_bloom_count_mismatch_returns_corrupt_error() {
    use crate::format::POSTSCRIPT_MAGIC;
    use crate::simd::bloom::Sbbf;

    let schema = Schema::try_new_arc(vec![
        Field::new("trace_id", DataType::FixedBytes16, false),
        Field::new("v", DataType::I64, false),
    ])
    .unwrap();

    let n = 8usize;
    let tids: Vec<[u8; 16]> = (0..n).map(|i| [(i + 1) as u8; 16]).collect();
    let vals: Vec<i64> = (0..n as i64).collect();
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(tids), None).unwrap()),
            Column::Flat(FlatColumn::try_new(FlatData::I64(vals), None).unwrap()),
        ],
        n,
    )
    .unwrap();

    // 1. Hot-tail BF02 with rgs.len() != num_row_groups rejected by hot_bloom & StatamReader::open.
    let mut hot_opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
    hot_opts.row_group_size = 4;
    hot_opts.mini_block_size = 4;
    hot_opts.compress_hot_tail_lz4 = false;
    hot_opts.bloom_cols = vec!["trace_id".to_owned()];
    hot_opts.hot_bloom_col = Some("trace_id".to_owned());

    let mut hot_bytes = Vec::new();
    let mut wh = StatamWriter::new(
        std::io::Cursor::new(&mut hot_bytes),
        Arc::clone(&schema),
        hot_opts,
        None,
    )
    .unwrap();
    wh.write_batch(&batch).unwrap();
    wh.finish().unwrap();

    let hot_reader =
        StatamReader::open(Arc::new(SliceReader::new(hot_bytes.clone())), None).unwrap();
    assert_eq!(hot_reader.meta().num_row_groups(), 2);
    let mut bad_meta = hot_reader.meta().clone();
    let mut single_rg = Sbbf::new_for_ndv(4, 0.01);
    single_rg.insert_bytes(&[1u8; 16]);
    let bad_bf02 = BloomIndex::PerRowGroup(vec![single_rg]).to_bytes();
    bad_meta.hot_bloom_bytes = Bytes::from(bad_bf02.clone());
    bad_meta.col_dir[0].bloom_offset = 0;
    bad_meta.col_dir[0].bloom_len = bad_bf02.len() as u32;
    assert!(matches!(
        bad_meta.hot_bloom(0),
        Err(StatamError::Corrupt {
            reason: "per-RG bloom count does not match num_row_groups",
        })
    ));

    // 2. Lazy BF02 with rgs.len() != num_row_groups rejected by check_blooms on scan.
    let mut lazy_opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
    lazy_opts.row_group_size = 4;
    lazy_opts.mini_block_size = 4;
    lazy_opts.compress_hot_tail_lz4 = false;
    lazy_opts.bloom_cols = vec!["trace_id".to_owned()];
    lazy_opts.hot_bloom_col = None;

    let mut lazy_bytes = Vec::new();
    let mut wl = StatamWriter::new(
        std::io::Cursor::new(&mut lazy_bytes),
        Arc::clone(&schema),
        lazy_opts,
        None,
    )
    .unwrap();
    wl.write_batch(&batch).unwrap();
    wl.finish().unwrap();

    let ps_off = lazy_bytes.len() - POSTSCRIPT_LEN;
    let mut ps = Postscript::decode(&lazy_bytes[ps_off..]).unwrap();
    assert_eq!(ps.flags & FLAG_HOT_TAIL_LZ4, 0);
    let orig_hot_body_start = lazy_bytes.len() - ps.hot_tail_len as usize;
    let orig_raw_body_len = ps.hot_tail_raw_len as usize;

    // Place the 1-RG BF02 right before the Hot Tail, clear hot_bloom flag on col 0,
    // and append a LazyIndexEntry pointing to the corrupt BF02.
    let mut raw_body = lazy_bytes[orig_hot_body_start..ps_off].to_vec();
    let col0_off = ps.col_dir_off as usize;
    let mut cd0 =
        ColumnDirEntry::decode(&raw_body[col0_off..col0_off + COLUMN_DIR_ENTRY_LEN]).unwrap();
    cd0.col_flags &= !COL_FLAG_BLOOM_IN_HOT_TAIL;
    cd0.bloom_offset = 0;
    cd0.bloom_len = bad_bf02.len() as u32;
    raw_body[col0_off..col0_off + COLUMN_DIR_ENTRY_LEN].copy_from_slice(&cd0.encode());

    let lazy_bloom_off = orig_hot_body_start as u64;
    let lazy_entry = LazyIndexEntry {
        phys_col_id: 0,
        kind: LazyIndexKind::Bloom,
        reserved: [0u8; 3],
        offset: lazy_bloom_off,
        len: bad_bf02.len() as u32,
        crc32c: crc32c::crc32c(&bad_bf02),
    };
    raw_body.extend_from_slice(&lazy_entry.encode());

    ps.lazy_dir_off = orig_raw_body_len as u32;
    ps.lazy_dir_len = LAZY_INDEX_ENTRY_LEN as u32;
    ps.hot_tail_raw_len = raw_body.len() as u32;
    ps.hot_tail_len = (raw_body.len() + POSTSCRIPT_LEN) as u32;
    ps.hot_tail_crc32c = crc32c::crc32c(&raw_body);
    let ps_encoded = ps.encode();
    assert_eq!(&ps_encoded[POSTSCRIPT_LEN - 4..], &POSTSCRIPT_MAGIC);

    let mut patched_lazy = Vec::new();
    patched_lazy.extend_from_slice(&lazy_bytes[..orig_hot_body_start]);
    patched_lazy.extend_from_slice(&bad_bf02);
    patched_lazy.extend_from_slice(&raw_body);
    patched_lazy.extend_from_slice(&ps_encoded);

    let lazy_reader = StatamReader::open(Arc::new(SliceReader::new(patched_lazy)), None).unwrap();
    let scan_res = lazy_reader.scan(&ScanRequest {
        id_lookups: vec![IdLookup {
            column: "trace_id".to_owned(),
            values: vec![vec![1u8; 16]],
        }],
        ..ScanRequest::default()
    });
    assert!(matches!(
        scan_res,
        Err(StatamError::Corrupt {
            reason: "per-RG bloom count does not match num_row_groups",
        })
    ));
}

#[test]
fn point_lookup_sparse_decode_parity_l0fast_and_l1compact() {
    let schema = Schema::try_new_arc(vec![
        Field::new("trace_id", DataType::FixedBytes16, false),
        Field::new("span_id", DataType::FixedBytes8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("attributes", DataType::MapUtf8, false),
        Field::new("tags", DataType::List(Box::new(DataType::Utf8)), false),
        Field::new("events", DataType::Events, false),
        Field::new("links", DataType::Links, false),
        Field::new("parent_span_id", DataType::FixedBytes8, true),
    ])
    .unwrap();

    let n = 512usize;
    let mut tids = Vec::with_capacity(n);
    let mut sids = Vec::with_capacity(n);
    let mut names = Vec::with_capacity(n);
    let mut map_offs = Vec::with_capacity(n + 1);
    map_offs.push(0u32);
    let mut m_keys = Vec::new();
    let mut m_vals = Vec::new();
    let mut list_offs = Vec::with_capacity(n + 1);
    list_offs.push(0u32);
    let mut list_vals = Vec::new();
    let mut ev_offs = Vec::with_capacity(n + 1);
    ev_offs.push(0u32);
    let mut ev_times = Vec::new();
    let mut ev_names = Vec::new();
    let mut ev_attrs = Vec::new();
    let mut lk_offs = Vec::with_capacity(n + 1);
    lk_offs.push(0u32);
    let mut lk_tids = Vec::new();
    let mut lk_sids = Vec::new();
    let mut lk_attrs = Vec::new();

    let target_tid = {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&42u64.to_le_bytes());
        b
    };

    for i in 0..n {
        let mut tid = [0u8; 16];
        let trace_group = if i == 17 || i == 259 {
            42usize
        } else {
            i + 100
        };
        tid[0..8].copy_from_slice(&(trace_group as u64).to_le_bytes());
        tids.push(tid);
        sids.push((i as u64).to_be_bytes());
        names.push(format!("operation-{i:04}-completion"));

        m_keys.push("env".to_owned());
        m_vals.push(if i % 2 == 0 {
            "prod".to_owned()
        } else {
            "dev".to_owned()
        });
        m_keys.push("model".to_owned());
        m_vals.push(format!("claude-3-5-sonnet-{i:03}"));
        if i % 4 == 0 || i == 17 || i == 259 {
            m_keys.push(format!("z_rare_{i}"));
            m_vals.push(format!("rare_payload_{i:04}"));
        }
        map_offs.push(m_keys.len() as u32);

        if i != 17 {
            list_vals.push("tag_one".to_owned());
            list_vals.push(format!("tag_{i}"));
        }
        list_offs.push(list_vals.len() as u32);

        if i != 259 {
            ev_times.push(1_700_000_000_000 + i as i64);
            ev_names.push("gen_ai.content");
            ev_attrs.push(format!("{{\"step\":{i}}}"));
        }
        ev_offs.push(ev_times.len() as u32);

        if i == 17 || i % 5 == 0 {
            lk_tids.push([i as u8; 16]);
            lk_sids.push([i as u8; 8]);
            lk_attrs.push(format!("{{\"link\":{i}}}"));
        }
        lk_offs.push(lk_tids.len() as u32);
    }

    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(tids), None).unwrap()),
            Column::Flat(FlatColumn::try_new(FlatData::FixedBytes8(sids), None).unwrap()),
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
                    FlatData::MapUtf8(Box::new(
                        MapData::try_new(
                            map_offs,
                            Utf8Data::try_from_slices(m_keys.iter().map(String::as_str)).unwrap(),
                            Utf8Data::try_from_slices(m_vals.iter().map(String::as_str)).unwrap(),
                            None,
                        )
                        .unwrap(),
                    )),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::List {
                        item_type: DataType::Utf8,
                        offsets: list_offs,
                        values: Box::new(Column::Flat(
                            FlatColumn::try_new(
                                FlatData::Utf8(
                                    Utf8Data::try_from_slices(list_vals.iter().map(String::as_str))
                                        .unwrap(),
                                ),
                                None,
                            )
                            .unwrap(),
                        )),
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Events(Box::new(
                        EventsData::try_new(
                            ev_offs,
                            ev_times,
                            Utf8Data::try_from_slices(ev_names).unwrap(),
                            Utf8Data::try_from_slices(ev_attrs.iter().map(String::as_str)).unwrap(),
                            None,
                        )
                        .unwrap(),
                    )),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Links(Box::new(
                        LinksData::try_new(
                            lk_offs,
                            lk_tids,
                            lk_sids,
                            Utf8Data::try_from_slices(lk_attrs.iter().map(String::as_str)).unwrap(),
                            None,
                        )
                        .unwrap(),
                    )),
                    None,
                )
                .unwrap(),
            ),
            Column::Const {
                value: ScalarValue::Null(DataType::FixedBytes8),
                len: n,
            },
        ],
        n,
    )
    .unwrap();

    for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
        let mut opts = WriteOptions::for_table(TableKind::Spans, profile);
        opts.mini_block_size = 512;
        let mut bytes = Vec::new();
        let mut w = StatamWriter::new(
            std::io::Cursor::new(&mut bytes),
            Arc::clone(&schema),
            opts,
            None,
        )
        .unwrap();
        w.write_batch(&batch).unwrap();
        w.finish().unwrap();

        let reader = StatamReader::open(Arc::new(SliceReader::new(bytes)), None).unwrap();
        let res = reader
            .scan(&ScanRequest {
                id_lookups: vec![IdLookup {
                    column: "trace_id".to_owned(),
                    values: vec![target_tid.to_vec()],
                }],
                ..ScanRequest::default()
            })
            .unwrap();

        assert_eq!(res.len(), 1);
        let out = &res[0];
        let active: Vec<usize> = match out.selection() {
            Selection::Mask(bm) => bm.iter_ones().collect(),
            _ => panic!("expected Selection::Mask"),
        };
        assert_eq!(active, vec![17, 259]);

        for (cidx, field) in schema.fields().iter().enumerate() {
            let expected_col = batch.columns()[cidx].materialize(None).unwrap();
            let got_col = out.columns()[cidx].materialize(None).unwrap();
            for &r in &active {
                assert_eq!(
                    got_col.is_null(r),
                    expected_col.is_null(r),
                    "profile {profile:?} col {} row {r}",
                    field.name
                );
                if !expected_col.is_null(r) {
                    assert_eq!(
                        got_col.get_scalar(r).unwrap(),
                        expected_col.get_scalar(r).unwrap(),
                        "profile {profile:?} col {} row {r}",
                        field.name
                    );
                }
            }
        }
    }
}

#[test]
fn inline_only_short_circuit_fast_path_matches_inline_and_validates_chunk_bounds() {
    let schema = Arc::new(
        Schema::try_new(vec![
            Field::new("id", DataType::I64, false),
            Field::new("input", DataType::Utf8, true),
        ])
        .unwrap(),
    );
    let pack_id = [0x91u8; 16];
    let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
    opts.payload_cols = vec!["input".to_owned()];
    opts.pack_id = Some(pack_id);
    opts.mini_block_size = 64;

    // Build rows with:
    // - row 0: Inline matching the rare needle
    // - row 1: Inline non-matching
    // - row 2: NULL
    // - rows 3..44: Chunked JSON arrays (emitting both TAG_KEYFRAME and TAG_DELTA)
    //   with enough unique bytes (>= 1024 B compressed) so TS02 + frame bloom are emitted.
    let mut ids = Vec::new();
    let mut inputs: Vec<Option<String>> = Vec::new();
    ids.push(0i64);
    inputs.push(Some("hit inline_rare_needle_xyz987 here".to_owned()));
    ids.push(1i64);
    inputs.push(Some("other short inline payload".to_owned()));
    ids.push(2i64);
    inputs.push(None);

    let shared_msg = format!(
        "{{\"role\":\"system\",\"content\":\"{}\"}}",
        "shared_prefix_token_".repeat(20)
    );
    let mut msgs = vec![shared_msg];
    let mut rng: u64 = 0x9e37_79b9_7f4a_7c15;
    for i in 3..44usize {
        let mut body = String::with_capacity(320);
        for _ in 0..32 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            // Limited alphabet ('a'..'f', '0'..'3') keeps trigram bloom density < 72%
            // while making Zstd compressed stored_len >= 1024 B!
            let ch = match (rng >> 24) % 10 {
                0..=5 => (b'a' + ((rng >> 24) % 6) as u8) as char,
                v => (b'0' + (v - 6) as u8) as char,
            };
            body.push(ch);
            body.push_str("_tok_");
        }
        msgs.push(format!(
            "{{\"role\":\"user\",\"turn\":{i:03},\"body\":\"{body}\"}}",
        ));
        ids.push(i as i64);
        inputs.push(Some(format!("[{}]", msgs.join(","))));
    }

    let input_refs: Vec<Option<&str>> = inputs.iter().map(Option::as_deref).collect();
    let mut val_bm = Bitmap::new_ones(inputs.len());
    let _ = val_bm.set(2, false);
    let input_utf8 = Utf8Data::try_from_slices(input_refs.iter().map(|o| o.unwrap_or(""))).unwrap();
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
            Column::Flat(FlatColumn::try_new(FlatData::Utf8(input_utf8), Some(val_bm)).unwrap()),
        ],
        inputs.len(),
    )
    .unwrap();

    let mut pb = ChunkPackBuilder::new(pack_id, test_pack_opts());
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

    let pack_s3 = Arc::new(SimulatedS3Reader::from_bytes(
        pack_bytes,
        SimulatedS3Config::default(),
    ));
    let pack_reader = Arc::new(ChunkPackReader::open(pack_s3.clone()).unwrap());
    assert!(
        pack_reader.postscript().trigram_len > 0,
        "pack must emit TS02 trigram section for short-circuit test"
    );
    let mut resolver = InMemoryPackResolver::new();
    resolver.insert(pack_id, Arc::clone(&pack_reader));
    let reader = StatamReader::open(
        Arc::new(SliceReader::new(file_bytes.clone())),
        Some(Arc::new(resolver)),
    )
    .unwrap();

    // Warm open is done; reset counters and verify ZERO .stpk data frames are read!
    pack_s3.reset_counters();
    let batches = reader
        .scan(&ScanRequest {
            projection: Some(vec![0]),
            substrings: vec![SubstringQuery {
                column: "input".to_owned(),
                needle: b"inline_rare_needle_xyz987".to_vec(),
                case_insensitive: false,
            }],
            ..ScanRequest::default()
        })
        .unwrap();
    let mut matched_ids = Vec::new();
    for b in &batches {
        let id_col = b.columns()[0].materialize(None).unwrap();
        let FlatData::I64(vals) = id_col.data() else {
            unreachable!();
        };
        match b.selection() {
            Selection::All => matched_ids.extend_from_slice(vals),
            Selection::Mask(bm) => {
                for r in bm.iter_ones() {
                    matched_ids.push(vals[r]);
                }
            }
            Selection::Indices(idxs) => {
                for &r in idxs {
                    matched_ids.push(vals[r as usize]);
                }
            }
        }
    }
    assert_eq!(matched_ids, vec![0]);
    assert!(
        pack_s3.counters().bytes_requested <= pack_reader.postscript().trigram_len as u64,
        "short-circuit fast path must read only TS02 trigram section (got {} bytes > {})",
        pack_s3.counters().bytes_requested,
        pack_reader.postscript().trigram_len
    );

    // Verify a short-circuiting pack with fewer chunks still rejects out-of-bounds chunk_id!
    let mut small_pb = ChunkPackBuilder::new(pack_id, test_pack_opts());
    let mut filler = Vec::with_capacity(4096);
    let mut st: u64 = 0x1122_3344_5566_7788;
    for _ in 0..4096 {
        st ^= st << 13;
        st ^= st >> 7;
        st ^= st << 17;
        filler.push(b'a' + ((st >> 24) as u8 % 4));
    }
    small_pb.intern_chunk(&filler).unwrap();
    let mut small_pack_bytes = Vec::new();
    small_pb.finish(&mut small_pack_bytes).unwrap();
    let small_pack =
        Arc::new(ChunkPackReader::open(Arc::new(SliceReader::new(small_pack_bytes))).unwrap());
    let mut small_resolver = InMemoryPackResolver::new();
    small_resolver.insert(pack_id, small_pack);
    let bad_reader = StatamReader::open(
        Arc::new(SliceReader::new(file_bytes)),
        Some(Arc::new(small_resolver)),
    )
    .unwrap();
    let err = bad_reader
        .scan(&ScanRequest {
            projection: Some(vec![0]),
            substrings: vec![SubstringQuery {
                column: "input".to_owned(),
                needle: b"inline_rare_needle_xyz987".to_vec(),
                case_insensitive: false,
            }],
            ..ScanRequest::default()
        })
        .unwrap_err();
    assert!(matches!(err, StatamError::Corrupt { .. }));
}

#[test]
fn parallel_read_threads_matches_single_threaded_on_substring_and_wide_point_lookup() {
    let spans_schema = make_spans_schema();
    let b0 = make_spans_batch(&spans_schema, 0, 160);
    let b1 = make_spans_batch(&spans_schema, 160, 160);
    let full_batch = Batch::concat(Arc::clone(&spans_schema), &[&b0, &b1], None).unwrap();

    // Pick a trace_id from the middle of the dataset for a wide point lookup.
    let target_tid = match full_batch.column(0).unwrap() {
        Column::Flat(fc) => match fc.data() {
            FlatData::FixedBytes16(v) => v[155],
            _ => panic!("expected FixedBytes16 trace_id"),
        },
        _ => panic!("expected flat trace_id"),
    };

    for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
        let mut opts = WriteOptions::for_table(TableKind::Spans, profile);
        opts.sort_cols = vec![2, 0];
        opts.row_group_size = 160;
        opts.mini_block_size = 80;

        let mut resolver = InMemoryPackResolver::new();
        let mut file_bytes = Vec::new();
        if profile == WriteProfile::L1Compact {
            let pack_id = [0x44u8; 16];
            opts.pack_id = Some(pack_id);
            let mut pack_opts = test_pack_opts();
            pack_opts.target_frame_raw_bytes = 512;
            let mut pb = ChunkPackBuilder::new(pack_id, pack_opts);
            let mut writer = StatamWriter::new(
                std::io::Cursor::new(&mut file_bytes),
                Arc::clone(&spans_schema),
                opts,
                Some(&mut pb),
            )
            .unwrap();
            writer.write_batch(&b0).unwrap();
            writer.write_batch(&b1).unwrap();
            writer.finish().unwrap();
            let mut pack_bytes = Vec::new();
            pb.finish(&mut pack_bytes).unwrap();
            resolver.insert_bytes(pack_id, pack_bytes).unwrap();
        } else {
            let mut writer = StatamWriter::new(
                std::io::Cursor::new(&mut file_bytes),
                Arc::clone(&spans_schema),
                opts,
                None,
            )
            .unwrap();
            writer.write_batch(&b0).unwrap();
            writer.write_batch(&b1).unwrap();
            writer.finish().unwrap();
        }

        let reader = StatamReader::open(
            Arc::new(SliceReader::new(file_bytes)),
            Some(Arc::new(resolver)),
        )
        .unwrap();

        // 1. Wide point lookup (exercises sparse PayloadRefs + parallel Phase-2 column decode)
        let point_seq = reader
            .scan(&ScanRequest {
                projection: None,
                id_lookups: vec![IdLookup::eq("trace_id", target_tid)],
                coalesce_mode: CoalesceMode::Point,
                read_threads: 1,
                ..ScanRequest::default()
            })
            .unwrap();
        let point_par = reader
            .scan(&ScanRequest {
                projection: None,
                id_lookups: vec![IdLookup::eq("trace_id", target_tid)],
                coalesce_mode: CoalesceMode::Point,
                read_threads: 4,
                ..ScanRequest::default()
            })
            .unwrap();
        let cat_seq = concat_scanned_batches(&spans_schema, &point_seq);
        let cat_par = concat_scanned_batches(&spans_schema, &point_par);
        assert!(cat_seq.num_rows() >= 1);
        assert_eq!(cat_seq.num_rows(), cat_par.num_rows());
        for c in 0..spans_schema.len() {
            assert!(
                cat_seq
                    .column(c)
                    .unwrap()
                    .semantic_eq(cat_par.column(c).unwrap()),
                "point lookup mismatch at col {c} for {profile:?}"
            );
        }

        // 2. Substring scan across .stpk / L0 payloads with read_threads=1 vs 4
        let sub_seq = reader
            .scan(&ScanRequest {
                projection: None,
                substrings: vec![SubstringQuery::ilike("input", b"\"role\":\"USER\"")],
                read_threads: 1,
                ..ScanRequest::default()
            })
            .unwrap();
        let sub_par = reader
            .scan(&ScanRequest {
                projection: None,
                substrings: vec![SubstringQuery::ilike("input", b"\"role\":\"USER\"")],
                read_threads: 4,
                ..ScanRequest::default()
            })
            .unwrap();
        let sub_cat_seq = concat_scanned_batches(&spans_schema, &sub_seq);
        let sub_cat_par = concat_scanned_batches(&spans_schema, &sub_par);
        assert_eq!(sub_cat_seq.num_rows(), 240);
        assert_eq!(sub_cat_seq.num_rows(), sub_cat_par.num_rows());
        for c in 0..spans_schema.len() {
            assert!(
                sub_cat_seq
                    .column(c)
                    .unwrap()
                    .semantic_eq(sub_cat_par.column(c).unwrap()),
                "substring mismatch at col {c} for {profile:?}"
            );
        }

        // 3. Duplicate projected Encoding::Dict column (also cached in Phase 1) with
        //    enough uncached columns to trigger parallel Phase-2 decode (`parallel_tasks >= 4`).
        let dup_proj = [4usize, 4, 0, 1, 2, 3, 5];
        let dup_schema = Arc::new(spans_schema.project(&dup_proj).unwrap());
        let dup_pred = Predicate::new(
            "status_code",
            ZonePred::Eq(ZoneValue::Bytes(b"OK".to_vec())),
        );
        let dup_seq = reader
            .scan(&ScanRequest {
                projection: Some(dup_proj.to_vec()),
                predicates: vec![dup_pred.clone()],
                read_threads: 1,
                ..ScanRequest::default()
            })
            .unwrap();
        let dup_par = reader
            .scan(&ScanRequest {
                projection: Some(dup_proj.to_vec()),
                predicates: vec![dup_pred],
                read_threads: 4,
                ..ScanRequest::default()
            })
            .unwrap();
        let dup_cat_seq = concat_scanned_batches(&dup_schema, &dup_seq);
        let dup_cat_par = concat_scanned_batches(&dup_schema, &dup_par);
        assert!(dup_cat_seq.num_rows() > 0);
        assert_eq!(dup_cat_seq.num_rows(), dup_cat_par.num_rows());
        for c in 0..dup_proj.len() {
            assert!(
                dup_cat_seq
                    .column(c)
                    .unwrap()
                    .semantic_eq(dup_cat_par.column(c).unwrap()),
                "duplicate dict col mismatch at pos {c} for {profile:?}"
            );
        }
    }
}

#[test]
fn take_rows_scattered_matches_full_scan() {
    let spans_schema = make_spans_schema();
    // 1,200 rows across 4 RGs and 8+ mini-blocks so 100 and 1,000 scattered indices
    // span every RG and mini-block boundary!
    let b0 = make_spans_batch(&spans_schema, 0, 600);
    let b1 = make_spans_batch(&spans_schema, 600, 600);

    for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
        let mut opts = WriteOptions::for_table(TableKind::Spans, profile);
        opts.sort_cols = vec![2, 0];
        opts.row_group_size = 300;
        opts.mini_block_size = 100;

        let mut resolver = InMemoryPackResolver::new();
        let mut file_bytes = Vec::new();
        if profile == WriteProfile::L1Compact {
            let pack_id = [0x55u8; 16];
            opts.pack_id = Some(pack_id);
            let mut pb = ChunkPackBuilder::new(pack_id, test_pack_opts());
            let mut writer = StatamWriter::new(
                std::io::Cursor::new(&mut file_bytes),
                Arc::clone(&spans_schema),
                opts,
                Some(&mut pb),
            )
            .unwrap();
            writer.write_batch(&b0).unwrap();
            writer.write_batch(&b1).unwrap();
            writer.finish().unwrap();
            let mut pack_bytes = Vec::new();
            pb.finish(&mut pack_bytes).unwrap();
            resolver.insert_bytes(pack_id, pack_bytes).unwrap();
        } else {
            let mut writer = StatamWriter::new(
                std::io::Cursor::new(&mut file_bytes),
                Arc::clone(&spans_schema),
                opts,
                None,
            )
            .unwrap();
            writer.write_batch(&b0).unwrap();
            writer.write_batch(&b1).unwrap();
            writer.finish().unwrap();
        }

        let reader = StatamReader::open(
            Arc::new(SliceReader::new(file_bytes)),
            Some(Arc::new(resolver)),
        )
        .unwrap();

        let full = concat_scanned_batches(
            &spans_schema,
            &reader.scan(&ScanRequest::default()).unwrap(),
        );
        assert_eq!(full.num_rows(), 1200);

        // Empty & out-of-bounds only
        assert!(reader.take_rows(&[], None, 1).unwrap().is_empty());
        assert!(reader.take_rows(&[1200, 9999], None, 1).unwrap().is_empty());

        // 100 scattered (unsorted + duplicates + out-of-bounds) indices
        let mut indices_100: Vec<u64> = (0..100u64).map(|i| (i * 73) % 1200).collect();
        indices_100.push(1199);
        indices_100.push(73); // duplicate
        indices_100.push(5000); // out of bounds

        let mut expected_100 = indices_100
            .iter()
            .copied()
            .filter(|&r| r < 1200)
            .collect::<Vec<_>>();
        expected_100.sort_unstable();
        expected_100.dedup();
        let expected_mask_100 = {
            let mut bm = Bitmap::new_zeroed(1200);
            for &r in &expected_100 {
                let _ = bm.set(r as usize, true);
            }
            bm
        };
        let masked_100 = Batch::try_new(
            Arc::clone(&spans_schema),
            full.columns().to_vec(),
            1200,
            Selection::Mask(expected_mask_100),
        )
        .unwrap();
        let expected_batch_100 =
            Batch::concat(Arc::clone(&spans_schema), &[&masked_100], None).unwrap();

        for threads in [1usize, 4] {
            let taken = reader.take_rows(&indices_100, None, threads).unwrap();
            for b in &taken {
                assert!(matches!(b.selection(), Selection::All));
            }
            let got = concat_scanned_batches(&spans_schema, &taken);
            assert_eq!(got.num_rows(), expected_100.len());
            for c in 0..spans_schema.len() {
                assert!(
                    got.column(c)
                        .unwrap()
                        .semantic_eq(expected_batch_100.column(c).unwrap()),
                    "take_rows(100, threads={threads}) mismatch at col {c} for {profile:?}"
                );
            }
        }

        // 1,000 scattered indices + projected subset + 0-col projection
        let indices_1000: Vec<u64> = (0..1000u64).map(|i| (i * 197 + 11) % 1200).collect();
        let mut unique_1000 = indices_1000.clone();
        unique_1000.sort_unstable();
        unique_1000.dedup();
        let expected_mask_1000 = {
            let mut bm = Bitmap::new_zeroed(1200);
            for &r in &unique_1000 {
                let _ = bm.set(r as usize, true);
            }
            bm
        };
        let masked_1000 = Batch::try_new(
            Arc::clone(&spans_schema),
            full.columns().to_vec(),
            1200,
            Selection::Mask(expected_mask_1000),
        )
        .unwrap();
        let expected_batch_1000 =
            Batch::concat(Arc::clone(&spans_schema), &[&masked_1000], None).unwrap();

        let taken_1000 = reader.take_rows(&indices_1000, None, 4).unwrap();
        let got_1000 = concat_scanned_batches(&spans_schema, &taken_1000);
        assert_eq!(got_1000.num_rows(), unique_1000.len());
        for c in 0..spans_schema.len() {
            assert!(
                got_1000
                    .column(c)
                    .unwrap()
                    .semantic_eq(expected_batch_1000.column(c).unwrap()),
                "take_rows(1000, threads=4) mismatch at col {c} for {profile:?}"
            );
        }

        // Narrow projection + 0-col projection
        let narrow_proj = [1usize, 3, 10, 13];
        let narrow_schema = Arc::new(spans_schema.project(&narrow_proj).unwrap());
        let taken_narrow = reader
            .take_rows(&indices_100, Some(&narrow_proj), 4)
            .unwrap();
        let got_narrow = concat_scanned_batches(&narrow_schema, &taken_narrow);
        assert_eq!(got_narrow.num_rows(), expected_100.len());
        for (pos, &orig_c) in narrow_proj.iter().enumerate() {
            assert!(
                got_narrow
                    .column(pos)
                    .unwrap()
                    .semantic_eq(expected_batch_100.column(orig_c).unwrap()),
                "take_rows narrow mismatch at col {orig_c}"
            );
        }

        let taken_zero = reader.take_rows(&indices_100, Some(&[]), 1).unwrap();
        for b in &taken_zero {
            assert!(matches!(b.selection(), Selection::All));
        }
        let zero_rows: usize = taken_zero.iter().map(Batch::num_rows).sum();
        assert_eq!(zero_rows, expected_100.len());
    }
}

#[test]
fn parallel_miniblock_scan_matches_single_threaded_across_multiple_mbs() {
    // Multi-MB dataset (>= 4 mini-blocks inside one RG) with:
    // - `ts`: sorted TimestampNs
    // - `url`: FSST-compressible non-empty URL strings (tautological `url <> ''`)
    // - `title`: Utf8 column queried with `LIKE`
    // - `region_id`: I64 bucketed metric
    let schema = Arc::new(
        Schema::try_new(vec![
            Field::new("ts", DataType::TimestampNs { utc: true }, false),
            Field::new("url", DataType::Utf8, false),
            Field::new("title", DataType::Utf8, false),
            Field::new("region_id", DataType::I64, false),
        ])
        .unwrap(),
    );

    let total_rows = 12_500usize; // > 3 * 4096 -> 4 mini-blocks in 1 RG
    let mut ts_vals = Vec::with_capacity(total_rows);
    let mut urls = Vec::with_capacity(total_rows);
    let mut titles = Vec::with_capacity(total_rows);
    let mut regions = Vec::with_capacity(total_rows);

    for i in 0..total_rows {
        ts_vals.push(1_700_000_000_000_000_000i64 + (i as i64) * 1_000_000);
        urls.push(format!(
            "https://cdn.example.org/assets/v2/tenant_{:02}/item_{:05}/index.html?ref=home",
            i % 17,
            i
        ));
        if i % 9 == 0 {
            titles.push(format!("Breaking News Special Report #{i}"));
        } else {
            titles.push(format!("Daily Digest Summary Entry #{i}"));
        }
        regions.push((i % 7) as i64);
    }

    let batch = Batch::try_new(
        Arc::clone(&schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: ts_vals,
                        utc: true,
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(urls.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(titles.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(FlatColumn::try_new(FlatData::I64(regions), None).unwrap()),
        ],
        total_rows,
        Selection::All,
    )
    .unwrap();

    let opts = WriteOptions {
        profile: WriteProfile::L1Compact,
        table_kind: TableKind::Spans,
        sort_cols: vec![0],
        mini_block_size: 4096,
        row_group_size: 65_536,
        ..WriteOptions::default()
    };
    let mut file_bytes = Vec::new();
    let mut writer = StatamWriter::new(
        std::io::Cursor::new(&mut file_bytes),
        Arc::clone(&schema),
        opts,
        None,
    )
    .unwrap();
    writer.write_batch(&batch).unwrap();
    writer.finish().unwrap();

    let s3 = Arc::new(SimulatedS3Reader::from_bytes(
        file_bytes,
        SimulatedS3Config::default(),
    ));
    let reader = StatamReader::open(Arc::clone(&s3) as Arc<dyn RangeReader>, None).unwrap();
    assert!(reader.meta().rg_blocks(0, 0).len() >= 4);
    // Confirm `url` uses FSST across mini-blocks so tautology + FSST decode both exercise.
    assert!(
        reader
            .meta()
            .rg_blocks(1, 0)
            .iter()
            .any(|b| b.encoding == Encoding::Fsst)
    );

    // 1. CB_Q24-style `url <> ''` unprojected (tautology skips reading `url` in Phase 1 & Phase 2).
    s3.reset_counters();
    let req_unproj = ScanRequest {
        projection: Some(vec![0]),
        predicates: vec![Predicate::new(
            "url",
            ZonePred::Ne(ZoneValue::Bytes(Vec::new())),
        )],
        coalesce_mode: CoalesceMode::Scan,
        read_threads: 1,
        ..ScanRequest::default()
    };
    let batches_unproj_1 = reader.scan(&req_unproj).unwrap();
    let unproj_bytes = s3.counters().bytes_requested;

    let req_unproj_4 = ScanRequest {
        read_threads: 4,
        ..req_unproj
    };
    let batches_unproj_4 = reader.scan(&req_unproj_4).unwrap();
    let proj_0_schema = Arc::new(schema.project(&[0]).unwrap());
    let cat_u1 = concat_scanned_batches(&proj_0_schema, &batches_unproj_1);
    let cat_u4 = concat_scanned_batches(&proj_0_schema, &batches_unproj_4);
    assert_eq!(cat_u1.num_rows(), total_rows);
    assert_eq!(cat_u4.num_rows(), total_rows);
    for c in 0..proj_0_schema.len() {
        assert!(
            cat_u1
                .column(c)
                .unwrap()
                .semantic_eq(cat_u4.column(c).unwrap())
        );
    }

    // Compare bytes requested when `url` IS projected (must be strictly larger than unprojected).
    s3.reset_counters();
    let req_proj_url = ScanRequest {
        projection: Some(vec![0, 1]),
        predicates: vec![Predicate::new(
            "url",
            ZonePred::Ne(ZoneValue::Bytes(Vec::new())),
        )],
        coalesce_mode: CoalesceMode::Scan,
        read_threads: 1,
        ..ScanRequest::default()
    };
    let _ = reader.scan(&req_proj_url).unwrap();
    let proj_bytes = s3.counters().bytes_requested;
    assert!(
        unproj_bytes < proj_bytes,
        "expected tautology url <> '' unprojected ({unproj_bytes}) < projected ({proj_bytes})"
    );

    // 2. Combined CB_Q24-style `url <> ''` + CB_Q23-style `LIKE` + TimeRange across Scan & Point modes.
    let tr = TimeRange::new(
        1_700_000_000_000_000_000i64 + 1_000 * 1_000_000,
        1_700_000_000_000_000_000i64 + 11_500 * 1_000_000,
    );
    for mode in [CoalesceMode::Scan, CoalesceMode::Point] {
        let req_1 = ScanRequest {
            projection: None,
            time_range: tr,
            predicates: vec![Predicate::new(
                "url",
                ZonePred::Ne(ZoneValue::Bytes(Vec::new())),
            )],
            substrings: vec![SubstringQuery::like("title", b"Special Report")],
            coalesce_mode: mode,
            read_threads: 1,
            ..ScanRequest::default()
        };
        let req_4 = ScanRequest {
            read_threads: 4,
            ..req_1.clone()
        };
        let b1 = reader.scan(&req_1).unwrap();
        let b4 = reader.scan(&req_4).unwrap();
        let c1 = concat_scanned_batches(&schema, &b1);
        let c4 = concat_scanned_batches(&schema, &b4);
        let expected_rows = (1000..=11_500).filter(|i| i % 9 == 0).count();
        assert_eq!(c1.num_rows(), expected_rows);
        assert_eq!(c4.num_rows(), expected_rows);
        for c in 0..schema.len() {
            assert!(
                c1.column(c).unwrap().semantic_eq(c4.column(c).unwrap()),
                "mode={mode:?} col={c} mismatch between read_threads=1 and 4"
            );
        }
    }
}

#[test]
fn raw_utf8_phase1_substring_parity_and_unicode_narrow_trigram_pruning() {
    use crate::codec::{WriteProfile, encode_mini_block};
    use crate::format::read_framed_block;

    let n = 128usize;
    let mut active = Bitmap::new_zeroed(n);
    for r in [0usize, 5, 17, 63, 64, 99, 127] {
        let _ = active.set(r, true);
    }

    // Helper to test parity between eval_utf8_block_substring_raw and full decode + eval_utf8_substring.
    let check_raw_parity = |col: &Column, profile: WriteProfile, dict: Option<&FileDict>| {
        let enc = encode_mini_block(col, &DataType::Utf8, profile, dict).unwrap();
        let mut framed = Vec::new();
        let entry = enc
            .write_framed_entry(0, n as u16, 0, 3, &mut framed)
            .unwrap();
        let raw = if entry.stored_len == 0 {
            Vec::new()
        } else {
            read_framed_block(&framed, MAX_BLOCK_RAW_BYTES).unwrap().2
        };
        let decoded = decode_mini_block(&raw, &entry, &DataType::Utf8, dict).unwrap();

        for (needle, ci) in [
            ("Google".as_bytes(), false),
            ("google".as_bytes(), true),
            ("Alienación".as_bytes(), true),
            ("ALIENACIÓN".as_bytes(), true),
            ("".as_bytes(), false),
            ("absent_token_xyz".as_bytes(), true),
        ] {
            let dict_bm = dict.map(|d| {
                let mut matcher = Utf8Matcher::new(needle, ci);
                let mut bm = Bitmap::new_zeroed(d.len());
                if let FlatData::Utf8(u) = d.values().data() {
                    for i in 0..u.len() {
                        if let Some(s) = u.get(i)
                            && matcher.matches(s.as_bytes())
                        {
                            let _ = bm.set(i, true);
                        }
                    }
                }
                bm
            });
            let expected = eval_utf8_substring(&decoded, needle, ci, dict_bm.as_ref()).unwrap();
            let expected_masked = expected.and(&active).unwrap();
            let got_raw = eval_utf8_block_substring_raw(
                &raw,
                &entry,
                n,
                &active,
                needle,
                ci,
                dict_bm.as_ref(),
                dict,
            )
            .unwrap();
            let got_masked = got_raw.and(&active).unwrap();
            assert_eq!(
                got_masked, expected_masked,
                "encoding={:?} ci={ci} mismatch",
                entry.encoding
            );
        }
    };

    // ConstNull, Const, Plain, Fsst, Rle(Plain), Rle(Fsst), Dict
    check_raw_parity(
        &Column::Const {
            value: ScalarValue::Null(DataType::Utf8),
            len: n,
        },
        WriteProfile::L1Compact,
        None,
    );
    check_raw_parity(
        &Column::Const {
            value: ScalarValue::Utf8("Google".to_owned()),
            len: n,
        },
        WriteProfile::L1Compact,
        None,
    );

    let mut strs = Vec::with_capacity(n);
    let mut valid = Vec::with_capacity(n);
    for i in 0..n {
        if i % 11 == 0 {
            strs.push(String::new());
            valid.push(false);
        } else if i % 5 == 0 {
            strs.push(format!(
                "{{\"title\":\"Google Search {i}\",\"tag\":\"Alienación\"}}"
            ));
            valid.push(true);
        } else {
            strs.push(format!(
                "{{\"title\":\"Other Entry {i}\",\"tag\":\"ALIENACIÓN {i}\"}}"
            ));
            valid.push(true);
        }
    }
    let flat_col = Column::Flat(
        FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(strs.iter().map(String::as_str)).unwrap()),
            Some(Bitmap::from_bools(&valid)),
        )
        .unwrap(),
    );
    check_raw_parity(&flat_col, WriteProfile::L0Fast, None);
    check_raw_parity(&flat_col, WriteProfile::L1Compact, None);

    // Rle(Plain) and Rle(Fsst)
    let mut rle_vals = Vec::new();
    let mut rle_valid = Vec::new();
    let mut rle_ends = Vec::new();
    for r in 0..16usize {
        rle_ends.push(((r + 1) * 8) as u32);
        if r == 3 {
            rle_vals.push(String::new());
            rle_valid.push(false);
        } else if r % 4 == 0 {
            rle_vals.push(format!(
                "Google News Run {r} Alienación long repeated payload {r}"
            ));
            rle_valid.push(true);
        } else {
            rle_vals.push(format!(
                "Plain Run {r} ALIENACIÓN long repeated payload {r}"
            ));
            rle_valid.push(true);
        }
    }
    let rle_col = Column::Rle(
        RleColumn::try_new(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(rle_vals.iter().map(String::as_str)).unwrap(),
                ),
                Some(Bitmap::from_bools(&rle_valid)),
            )
            .unwrap(),
            rle_ends,
        )
        .unwrap(),
    );
    check_raw_parity(&rle_col, WriteProfile::L0Fast, None);
    check_raw_parity(&rle_col, WriteProfile::L1Compact, None);

    // Dict
    let dict = FileDict::from_flat(
        FlatColumn::try_new(
            FlatData::Utf8(
                Utf8Data::try_from_slices(["ALIENACIÓN", "Alienación", "Google", "other"]).unwrap(),
            ),
            None,
        )
        .unwrap(),
        true,
    )
    .unwrap();
    let mut d_strs = Vec::with_capacity(n);
    let mut d_valid = Vec::with_capacity(n);
    for i in 0..n {
        if i % 13 == 0 {
            d_strs.push(String::new());
            d_valid.push(false);
        } else {
            d_strs.push(["Google", "Alienación", "ALIENACIÓN", "other"][i % 4].to_owned());
            d_valid.push(true);
        }
    }
    let dict_col = Column::Flat(
        FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(d_strs.iter().map(String::as_str)).unwrap()),
            Some(Bitmap::from_bools(&d_valid)),
        )
        .unwrap(),
    );
    check_raw_parity(&dict_col, WriteProfile::L1Compact, Some(&dict));

    // End-to-end StatamReader query with narrow trigram index + Unicode ILIKE + projection.
    let schema = Arc::new(
        Schema::try_new(vec![
            Field::new("ts", DataType::TimestampNs { utc: true }, false),
            Field::new("title", DataType::Utf8, false),
        ])
        .unwrap(),
    );
    let total = 2000usize;
    let mut ts_vals = Vec::with_capacity(total);
    let mut titles = Vec::with_capacity(total);
    let mut expected_rows = Vec::new();
    for i in 0..total {
        ts_vals.push(1_700_000_000_000_000_000i64 + i as i64 * 1_000_000);
        if i == 1500 {
            titles.push("Proyecto Alienación Especial - 1500".to_owned());
            expected_rows.push(i);
        } else if i == 1501 {
            titles.push("PROYECTO ALIENACIÓN TOTAL - 1501".to_owned());
            expected_rows.push(i);
        } else {
            titles.push(format!("qqq mmm qqq mmm title {i:04}"));
        }
    }
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: ts_vals,
                        utc: true,
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(titles.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            ),
        ],
        total,
    )
    .unwrap();

    let opts = WriteOptions {
        profile: WriteProfile::L1Compact,
        mini_block_size: 256,
        row_group_size: 2000,
        narrow_trigram_cols: vec!["title".to_owned()],
        ..WriteOptions::default()
    };
    let mut file_bytes = Vec::new();
    let mut writer = StatamWriter::new(
        std::io::Cursor::new(&mut file_bytes),
        Arc::clone(&schema),
        opts,
        None,
    )
    .unwrap();
    writer.write_batch(&batch).unwrap();
    writer.finish().unwrap();

    let s3 = Arc::new(SimulatedS3Reader::from_bytes(
        file_bytes,
        SimulatedS3Config::default(),
    ));
    let reader = StatamReader::open(s3 as Arc<dyn RangeReader>, None).unwrap();
    for needle in ["Alienación".as_bytes(), "ALIENACIÓN".as_bytes()] {
        let req = ScanRequest {
            projection: Some(vec![0, 1]),
            substrings: vec![SubstringQuery::ilike("title", needle)],
            ..ScanRequest::default()
        };
        let scanned = reader.scan(&req).unwrap();
        let combined = concat_scanned_batches(&schema, &scanned);
        assert_eq!(combined.num_rows(), expected_rows.len());
        let FlatData::TimestampNs { values, .. } = combined
            .column(0)
            .unwrap()
            .materialize(None)
            .unwrap()
            .data()
            .clone()
        else {
            panic!("expected TimestampNs");
        };
        let expected_ts: Vec<i64> = expected_rows
            .iter()
            .map(|&i| 1_700_000_000_000_000_000i64 + i as i64 * 1_000_000)
            .collect();
        assert_eq!(values, expected_ts);
    }
}

#[test]
fn raw_utf8_block_substring_rejects_corrupt_const_plain_fsst_and_dict() {
    use crate::codec::{WriteProfile, encode_mini_block};
    use crate::format::read_framed_block;

    let n = 16usize;
    let active = Bitmap::new_ones(n);

    // 1. Const: reject BLOCK_FLAG_CONST_EMPTY_COLLECTION and invalid UTF-8.
    let mut const_entry = BlockEntry {
        first_row: 0,
        row_count: n as u16,
        null_count: 0,
        offset: 0,
        stored_len: 0,
        raw_len: 0,
        encoding: Encoding::Const,
        codec: Codec::None,
        block_flags: BLOCK_FLAG_CONST_EMPTY_COLLECTION,
        min_inline_len: 0,
        max_inline_len: 0,
        reserved: [0; 3],
        min_inline: [0; 16],
        max_inline: [0; 16],
    };
    assert!(matches!(
        eval_utf8_block_substring_raw(&[], &const_entry, n, &active, b"", false, None, None),
        Err(StatamError::Corrupt { .. })
    ));
    const_entry.block_flags = 0;
    const_entry.min_inline_len = 2;
    const_entry.min_inline[..2].copy_from_slice(&[0xFF, 0xFE]);
    assert!(matches!(
        eval_utf8_block_substring_raw(&[], &const_entry, n, &active, b"", false, None, None),
        Err(StatamError::Corrupt { .. })
    ));

    // 2. Plain & FSST: truncated body must be rejected even when needle is empty!
    let plain_vals: Vec<String> = (0..n).map(|i| format!("alpha_row_{i:02}")).collect();
    let plain_col = Column::Flat(
        FlatColumn::try_new(
            FlatData::Utf8(
                Utf8Data::try_from_slices(plain_vals.iter().map(String::as_str)).unwrap(),
            ),
            None,
        )
        .unwrap(),
    );
    let plain_enc =
        encode_mini_block(&plain_col, &DataType::Utf8, WriteProfile::L0Fast, None).unwrap();
    let mut plain_framed = Vec::new();
    let plain_entry = plain_enc
        .write_framed_entry(0, n as u16, 0, 3, &mut plain_framed)
        .unwrap();
    assert_eq!(plain_entry.encoding, Encoding::Plain);
    let plain_raw = read_framed_block(&plain_framed, MAX_BLOCK_RAW_BYTES)
        .unwrap()
        .2;
    assert!(matches!(
        eval_utf8_block_substring_raw(
            &plain_raw[..4],
            &plain_entry,
            n,
            &active,
            b"",
            false,
            None,
            None
        ),
        Err(StatamError::Corrupt { .. })
    ));

    let fsst_vals: Vec<String> = (0..64)
        .map(|i| format!("{{\"role\":\"assistant\",\"message\":\"hello world {i:04}\"}}"))
        .collect();
    let fsst_col = Column::Flat(
        FlatColumn::try_new(
            FlatData::Utf8(
                Utf8Data::try_from_slices(fsst_vals.iter().map(String::as_str)).unwrap(),
            ),
            None,
        )
        .unwrap(),
    );
    let fsst_enc =
        encode_mini_block(&fsst_col, &DataType::Utf8, WriteProfile::L1Compact, None).unwrap();
    let mut fsst_framed = Vec::new();
    let fsst_entry = fsst_enc
        .write_framed_entry(0, 64, 0, 3, &mut fsst_framed)
        .unwrap();
    assert_eq!(fsst_entry.encoding, Encoding::Fsst);
    let fsst_raw = read_framed_block(&fsst_framed, MAX_BLOCK_RAW_BYTES)
        .unwrap()
        .2;
    let active64 = Bitmap::new_ones(64);
    assert!(matches!(
        eval_utf8_block_substring_raw(
            &fsst_raw[..8],
            &fsst_entry,
            64,
            &active64,
            b"",
            false,
            None,
            None
        ),
        Err(StatamError::Corrupt { .. })
    ));

    // 3. Dict: missing FileDict rejected; out-of-bounds code rejected even for empty needle,
    // all_zeros code_bm, and all_ones code_bm.
    let dict = FileDict::from_flat(
        FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["alpha", "beta"]).unwrap()),
            None,
        )
        .unwrap(),
        true,
    )
    .unwrap();
    let dict_vals: Vec<&str> = (0..n).map(|i| ["alpha", "beta"][i % 2]).collect();
    let dict_col = Column::Flat(
        FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(dict_vals).unwrap()),
            None,
        )
        .unwrap(),
    );
    let dict_enc = encode_mini_block(
        &dict_col,
        &DataType::Utf8,
        WriteProfile::L1Compact,
        Some(&dict),
    )
    .unwrap();
    let mut dict_framed = Vec::new();
    let dict_entry = dict_enc
        .write_framed_entry(0, n as u16, 0, 3, &mut dict_framed)
        .unwrap();
    assert_eq!(dict_entry.encoding, Encoding::Dict);
    let dict_raw = read_framed_block(&dict_framed, MAX_BLOCK_RAW_BYTES)
        .unwrap()
        .2;

    // Missing FileDict rejected.
    assert!(matches!(
        eval_utf8_block_substring_raw(&dict_raw, &dict_entry, n, &active, b"", false, None, None),
        Err(StatamError::Corrupt {
            reason: "Dict mini-block requires FileDict",
        })
    ));

    // Craft a dict block body with out-of-bounds code 99 (>= dict.len() == 2).
    let mut bad_dict_raw = Vec::new();
    bad_dict_raw.push(0u8); // sub_enc = ForBitpack
    bad_dict_raw.extend_from_slice(&99u32.to_le_bytes()); // base = 99
    bad_dict_raw.push(0u8); // bw = 0
    let all_zeros_bm = Bitmap::new_zeroed(dict.len());
    let all_ones_bm = Bitmap::new_ones(dict.len());
    for (needle, bm) in [
        (b"".as_slice(), None),
        (b"zz".as_slice(), Some(&all_zeros_bm)),
        (b"a".as_slice(), Some(&all_ones_bm)),
    ] {
        assert!(matches!(
            eval_utf8_block_substring_raw(
                &bad_dict_raw,
                &dict_entry,
                n,
                &active,
                needle,
                false,
                bm,
                Some(&dict),
            ),
            Err(StatamError::Corrupt {
                reason: "dict code out of bounds",
            })
        ));
    }
}

#[test]
fn reader_rejects_multi_chunk_payload_referencing_pack_without_edge_table() {
    use crate::payload::pack::ChunkPackBuilder;
    use crate::payload::{PayloadRefRow, encode_payload_refs};

    let pack_id = [0xD4u8; 16];
    let schema = Arc::new(
        Schema::try_new(vec![
            Field::new("ts", DataType::TimestampNs { utc: true }, false),
            Field::new("body", DataType::Utf8, true),
        ])
        .unwrap(),
    );
    // Build a pseudo-random > 16 KiB payload so StatamWriter splits it into >= 2 chunks in `.stpk`.
    let mut state = 0x1357_2468_abcd_ef01_u64;
    let mut big = String::with_capacity(20 * 1024);
    for _ in 0..(20 * 1024) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        big.push((b'a' + ((state >> 24) as u8 % 4)) as char);
    }
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: vec![1_700_000_000_000_000_000, 1_700_000_000_001_000_000],
                        utc: true,
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["short_ok", big.as_str()]).unwrap()),
                    None,
                )
                .unwrap(),
            ),
        ],
        2,
    )
    .unwrap();

    let pack_opts = PackWriteOptions {
        target_frame_raw_bytes: 8 * 1024,
        codec: Codec::None,
        zstd_level: 1,
        enable_trigram_skip: true,
        compress_toc_lz4: false,
        write_threads: 1,
        ..PackWriteOptions::default()
    };
    let mut pb = ChunkPackBuilder::new(pack_id, pack_opts);
    let opts = WriteOptions {
        profile: WriteProfile::L1Compact,
        payload_cols: vec!["body".to_owned()],
        pack_id: Some(pack_id),
        ..WriteOptions::default()
    };
    let mut file_bytes = Vec::new();
    let mut writer = StatamWriter::new(
        std::io::Cursor::new(&mut file_bytes),
        Arc::clone(&schema),
        opts,
        Some(&mut pb),
    )
    .unwrap();
    writer.write_batch(&batch).unwrap();
    writer.finish().unwrap();
    let mut valid_pack_bytes = Vec::new();
    pb.finish(&mut valid_pack_bytes).unwrap();
    let valid_pack = ChunkPackReader::open(Arc::new(SliceReader::new(valid_pack_bytes))).unwrap();
    let chunk_count = valid_pack.toc().chunks.len();
    assert!(chunk_count >= 2);

    // Build a paired `.stpk` with the exact same `pack_id` and `chunk_count` but `edge_table_len == 0`.
    let mut no_edge_pb = ChunkPackBuilder::new(pack_id, pack_opts);
    no_edge_pb.enable_boundary_tracking();
    for cid in 0..chunk_count as u32 {
        let mut c = Vec::with_capacity(4096);
        c.extend_from_slice(&cid.to_le_bytes());
        for _ in 4..4096 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            c.push(b'a' + ((state >> 24) as u8 % 4));
        }
        no_edge_pb.intern_chunk(&c).unwrap();
    }
    let mut no_edge_bytes = Vec::new();
    no_edge_pb.finish(&mut no_edge_bytes).unwrap();
    let no_edge_pack =
        Arc::new(ChunkPackReader::open(Arc::new(SliceReader::new(no_edge_bytes))).unwrap());
    assert_eq!(no_edge_pack.postscript().edge_table_len, 0);

    let mut resolver = InMemoryPackResolver::new();
    resolver.insert(pack_id, Arc::clone(&no_edge_pack));
    let reader = StatamReader::open(
        Arc::new(SliceReader::new(file_bytes)),
        Some(Arc::new(resolver)),
    )
    .unwrap();

    // End-to-end StatamReader::scan takes the inline short-circuit path (`scan_payload_refs_inline_substring`)
    // and must reject the 2-chunk row referencing the edge-table-less pack.
    let err_scan = reader
        .scan(&ScanRequest {
            projection: Some(vec![0]),
            substrings: vec![SubstringQuery::like("body", "XYZ_ABSENT_999")],
            ..ScanRequest::default()
        })
        .unwrap_err();
    assert!(matches!(
        err_scan,
        StatamError::Corrupt {
            reason: "multi-chunk row references pack without edge table",
        }
    ));

    // Direct `eval_payload_refs_substring` short-circuit loop also rejects the 2-chunk row.
    let mut ref_bytes = Vec::new();
    encode_payload_refs(
        &[
            PayloadRefRow::Inline(b"short_ok".to_vec()),
            PayloadRefRow::Chunks(vec![(0, 0), (0, 1)]),
        ],
        &mut ref_bytes,
    )
    .unwrap();
    let crafted_flat = decode_payload_refs_flat(&ref_bytes, 2, MAX_BATCH_DECODED_BYTES).unwrap();
    let mask = Bitmap::new_ones(2);
    let err_eval = reader
        .eval_payload_refs_substring(&crafted_flat, &mask, b"XYZ_ABSENT_999", false, None, 1)
        .unwrap_err();
    assert!(matches!(
        err_eval,
        StatamError::Corrupt {
            reason: "multi-chunk row references pack without edge table",
        }
    ));
}

#[test]
fn take_rows_random_access_schema_compact_selection_all_and_min_savings_parity() {
    let schema = Arc::new(
        Schema::try_new(vec![
            Field::new("ts", DataType::TimestampNs { utc: true }, false),
            Field::new("bucket", DataType::I64, true),
            Field::new("score", DataType::F64, false),
            Field::new("service", DataType::Utf8, true),
            Field::new("msg", DataType::Utf8, true),
            Field::new("ok", DataType::Bool, true),
        ])
        .unwrap(),
    );

    let n = 4200usize; // > 2048 rows across multiple mini-blocks and 2 RGs
    let mut ts_vals = Vec::with_capacity(n);
    let mut bucket_vals = Vec::with_capacity(n);
    let mut bucket_valid = Vec::with_capacity(n);
    let mut score_vals = Vec::with_capacity(n);
    let mut service_strs = Vec::with_capacity(n);
    let mut service_valid = Vec::with_capacity(n);
    let mut msg_strs = Vec::with_capacity(n);
    let mut msg_valid = Vec::with_capacity(n);
    let mut ok_bools = Vec::with_capacity(n);
    let mut ok_valid = Vec::with_capacity(n);

    let services = [
        "api-gateway",
        "auth-svc",
        "billing-worker",
        "llm-router",
        "eval-runner",
    ];
    let mut rng: u64 = 0x9e37_79b9_7f4a_7c15;
    for i in 0..n {
        ts_vals.push(1_700_000_000_000_000 + (i as i64) * 1_000_000);
        bucket_vals.push(((i * 7919) % 5000) as i64);
        bucket_valid.push(i % 13 != 0);

        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        // High-entropy finite float (mantissa + bounded exponent).
        let bits = (0x3ff0_0000_0000_0000u64) | (rng & 0x000f_ffff_ffff_ffffu64);
        score_vals.push(f64::from_bits(bits));

        service_strs.push(services[i % services.len()]);
        service_valid.push(i % 17 != 0);

        msg_strs.push(format!(
            "{{\"service\":\"{}\",\"status\":\"ok\",\"req\":\"req-{:06x}\",\"i\":{i}}}",
            services[i % services.len()],
            (i * 2654435761) & 0xff_ffff
        ));
        msg_valid.push(i % 19 != 0);

        ok_bools.push(i % 3 != 0);
        ok_valid.push(i % 11 != 0);
    }

    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: ts_vals,
                        utc: true,
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::I64(bucket_vals),
                    Some(Bitmap::from_bools(&bucket_valid)),
                )
                .unwrap(),
            ),
            Column::Flat(FlatColumn::try_new(FlatData::F64(score_vals), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(service_strs).unwrap()),
                    Some(Bitmap::from_bools(&service_valid)),
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(msg_strs.iter().map(String::as_str)).unwrap(),
                    ),
                    Some(Bitmap::from_bools(&msg_valid)),
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Bool(Bitmap::from_bools(&ok_bools)),
                    Some(Bitmap::from_bools(&ok_valid)),
                )
                .unwrap(),
            ),
        ],
        n,
    )
    .unwrap();

    for write_threads in [1usize, 4] {
        let mut opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L1Compact);
        opts.sort_cols = vec![0];
        opts.row_group_size = 2100;
        opts.mini_block_size = 1050;
        opts.write_threads = write_threads;

        let mut file_bytes = Vec::new();
        let mut writer = StatamWriter::new(
            std::io::Cursor::new(&mut file_bytes),
            Arc::clone(&schema),
            opts,
            None,
        )
        .unwrap();
        writer.write_batch(&batch).unwrap();
        writer.finish().unwrap();

        let reader = StatamReader::open(Arc::new(SliceReader::new(file_bytes)), None).unwrap();

        // Verify encodings & min_savings_pct guard on high-entropy Plain F64 blocks.
        assert!(
            reader.meta.rg_blocks(0, 0)[0].encoding == Encoding::DeltaBitpack,
            "ts should be DeltaBitpack"
        );
        assert!(
            reader.meta.rg_blocks(1, 0)[0].encoding == Encoding::ForBitpack,
            "bucket should be ForBitpack"
        );
        let score_blk = &reader.meta.rg_blocks(2, 0)[0];
        assert_eq!(score_blk.encoding, Encoding::Plain);
        assert_eq!(
            score_blk.codec,
            Codec::None,
            "high-entropy F64 block should fall back to Codec::None via 12% savings guard"
        );
        assert_eq!(reader.meta.rg_blocks(3, 0)[0].encoding, Encoding::Dict);
        assert_eq!(reader.meta.rg_blocks(4, 0)[0].encoding, Encoding::Fsst);
        assert_eq!(
            reader.meta.rg_blocks(5, 0)[0].encoding,
            Encoding::BitpackBool
        );

        let full = concat_scanned_batches(&schema, &reader.scan(&ScanRequest::default()).unwrap());
        assert_eq!(full.num_rows(), n);

        let indices_100: Vec<u64> = (0..100u64).map(|i| (i * 41 + 7) % (n as u64)).collect();
        let mut sorted_unique = indices_100.clone();
        sorted_unique.sort_unstable();
        sorted_unique.dedup();
        let expected_mask = {
            let mut bm = Bitmap::new_zeroed(n);
            for &r in &sorted_unique {
                let _ = bm.set(r as usize, true);
            }
            bm
        };
        let masked = Batch::try_new(
            Arc::clone(&schema),
            full.columns().to_vec(),
            n,
            Selection::Mask(expected_mask),
        )
        .unwrap();
        let expected = Batch::concat(Arc::clone(&schema), &[&masked], None).unwrap();

        for read_threads in [1usize, 4] {
            let taken = reader.take_rows(&indices_100, None, read_threads).unwrap();
            for b in &taken {
                assert!(matches!(b.selection(), Selection::All));
            }
            let got = concat_scanned_batches(&schema, &taken);
            assert_eq!(got.num_rows(), sorted_unique.len());
            for c in 0..schema.len() {
                assert!(
                    got.column(c)
                        .unwrap()
                        .semantic_eq(expected.column(c).unwrap()),
                    "random_access take_rows mismatch col={c} wt={write_threads} rt={read_threads}"
                );
            }

            let taken_zero = reader
                .take_rows(&indices_100, Some(&[]), read_threads)
                .unwrap();
            for b in &taken_zero {
                assert!(matches!(b.selection(), Selection::All));
            }
            let zero_rows: usize = taken_zero.iter().map(Batch::num_rows).sum();
            assert_eq!(zero_rows, sorted_unique.len());
        }
    }
}

#[test]
fn b4_point_mode_mixed_shredded_and_residual_or_is_not_null_map_predicates() {
    let schema = Arc::new(
        Schema::try_new(vec![
            Field::new("id", DataType::I64, false),
            Field::new("attributes", DataType::MapUtf8, true),
        ])
        .unwrap(),
    );
    // 32 rows: "k1" is dense (promoted, res_count == 0); "k2" is rare (stays in residual); row 31 is NULL.
    let mut ids = Vec::with_capacity(32);
    let mut entries: Vec<Option<Vec<(String, String)>>> = Vec::with_capacity(32);
    for i in 0..32i64 {
        ids.push(i);
        if i == 31 {
            entries.push(None);
        } else if i == 5 {
            entries.push(Some(vec![
                ("k1".to_owned(), "v1".to_owned()),
                ("k2".to_owned(), "v2".to_owned()),
            ]));
        } else {
            entries.push(Some(vec![("k1".to_owned(), "v1".to_owned())]));
        }
    }
    let mut map_offsets = vec![0u32];
    let mut key_offsets = vec![0u32];
    let mut key_bytes = Vec::new();
    let mut val_offsets = vec![0u32];
    let mut val_bytes = Vec::new();
    let mut val_nulls = Bitmap::new_zeroed(32);
    let mut total_entries = 0u32;
    for (r, opt) in entries.iter().enumerate() {
        if let Some(kvs) = opt {
            let _ = val_nulls.set(r, true);
            for (k, v) in kvs {
                key_bytes.extend_from_slice(k.as_bytes());
                key_offsets.push(key_bytes.len() as u32);
                val_bytes.extend_from_slice(v.as_bytes());
                val_offsets.push(val_bytes.len() as u32);
                total_entries += 1;
            }
        }
        map_offsets.push(total_entries);
    }
    let keys_u = Utf8Data::try_new(key_offsets, key_bytes).unwrap();
    let vals_u = Utf8Data::try_new(val_offsets, val_bytes).unwrap();
    let map_data = MapData::try_new(map_offsets, keys_u, vals_u, None).unwrap();
    let map_col = Column::Flat(
        FlatColumn::try_new(FlatData::MapUtf8(Box::new(map_data)), Some(val_nulls)).unwrap(),
    );
    let id_col = Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap());
    let batch = Batch::try_new_all(Arc::clone(&schema), vec![id_col, map_col], 32).unwrap();

    let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
    opts.mini_block_size = 16; // MB 0 has 0 nulls; MB 1 has 1 null (row 31).
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
    let sm = reader
        .meta()
        .schema_meta
        .shredded_maps
        .iter()
        .find(|s| s.logical_col_idx == 1)
        .expect("attributes must be shredded");
    let k1 = sm.keys.iter().find(|k| k.key == "k1").expect("k1 promoted");
    assert_eq!(k1.residual_count, 0);
    assert!(sm.keys.iter().all(|k| k.key != "k2"));

    // (a) Point mode: k1 = 'v1' AND unshredded k2 = 'v2' with map_keys: [("attributes", "k1")].
    let out_a = reader
        .scan(&ScanRequest {
            coalesce_mode: CoalesceMode::Point,
            map_keys: vec![("attributes".to_owned(), "k1".to_owned())],
            predicates: vec![
                Predicate {
                    column: "attributes".to_owned(),
                    map_key: Some("k1".to_owned()),
                    pred: ZonePred::Eq(ZoneValue::Bytes(b"v1".to_vec())),
                },
                Predicate {
                    column: "attributes".to_owned(),
                    map_key: Some("k2".to_owned()),
                    pred: ZonePred::Eq(ZoneValue::Bytes(b"v2".to_vec())),
                },
            ],
            ..ScanRequest::default()
        })
        .unwrap();
    let got_a = concat_scanned_batches(&schema, &out_a);
    assert_eq!(got_a.num_rows(), 1);
    assert_eq!(
        got_a.column(0).unwrap().get_scalar(0).unwrap(),
        ScalarValue::I64(5)
    );

    // (b) Point mode: k1 = 'v1' AND attributes IS NOT NULL.
    let out_b = reader
        .scan(&ScanRequest {
            coalesce_mode: CoalesceMode::Point,
            map_keys: vec![("attributes".to_owned(), "k1".to_owned())],
            predicates: vec![
                Predicate {
                    column: "attributes".to_owned(),
                    map_key: Some("k1".to_owned()),
                    pred: ZonePred::Eq(ZoneValue::Bytes(b"v1".to_vec())),
                },
                Predicate {
                    column: "attributes".to_owned(),
                    map_key: None,
                    pred: ZonePred::IsNotNull,
                },
            ],
            ..ScanRequest::default()
        })
        .unwrap();
    let got_b = concat_scanned_batches(&schema, &out_b);
    assert_eq!(got_b.num_rows(), 31);
}

#[test]
fn b7_l0_aliased_chunks_budget_and_streaming_l0_substring_scan() {
    // 1. Adversarial ParsedL0Payload with 1 4 MiB frame and 65 aliased ChunkLocs (260 MiB > 256 MiB).
    let raw_len = 4 * 1024 * 1024usize;
    let frame_raw = vec![b'B'; raw_len];
    let mut framed = Vec::new();
    let info =
        crate::format::write_framed_pack_frame(&frame_raw, Codec::Lz4, 0, 3, &mut framed).unwrap();
    let chunk_count = 65u32;
    let header_len = 12 + 16 + chunk_count as usize * 12;
    let mut l0_buf = Vec::with_capacity(header_len + framed.len());
    l0_buf.extend_from_slice(&L0_PAYLOAD_DIR_MAGIC);
    l0_buf.extend_from_slice(&1u32.to_le_bytes());
    l0_buf.extend_from_slice(&chunk_count.to_le_bytes());
    l0_buf.extend_from_slice(&(header_len as u64).to_le_bytes());
    l0_buf.extend_from_slice(&info.stored_len.to_le_bytes());
    l0_buf.extend_from_slice(&info.raw_len.to_le_bytes());
    for _ in 0..chunk_count {
        l0_buf.extend_from_slice(
            &ChunkLoc {
                frame_id: 0,
                offset_in_frame: 0,
                raw_len: raw_len as u32,
            }
            .encode(),
        );
    }
    l0_buf.extend_from_slice(&framed);

    let parsed = ParsedL0Payload::parse(&l0_buf).unwrap();
    let all_cids: Vec<u32> = (0..chunk_count).collect();
    assert!(matches!(
        parsed.read_chunks(&all_cids),
        Err(StatamError::BoundsExceeded { .. })
    ));

    // 2. Multi-frame L0 file scanned via streaming frame-at-a-time substring search (including Greek Final Sigma).
    let schema = Arc::new(
        Schema::try_new(vec![
            Field::new("id", DataType::I64, false),
            Field::new("body", DataType::Utf8, true),
        ])
        .unwrap(),
    );
    let mut ids = Vec::new();
    let mut body_offs = vec![0u32];
    let mut body_bytes = Vec::new();
    for i in 0..10i64 {
        ids.push(i);
        let s = if i == 4 {
            format!(
                "{}ΟΔΥΣΣΕΥΣ_TARGET_{}",
                "a".repeat(20_000),
                "b".repeat(20_000)
            )
        } else {
            format!("row_{i:02}_filler_").repeat(2_000)
        };
        body_bytes.extend_from_slice(s.as_bytes());
        body_offs.push(body_bytes.len() as u32);
    }
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_new(body_offs, body_bytes).unwrap()),
                    None,
                )
                .unwrap(),
            ),
        ],
        10,
    )
    .unwrap();
    let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
    opts.payload_cols = vec!["body".to_owned()];
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
    let out = reader
        .scan(&ScanRequest {
            substrings: vec![SubstringQuery {
                column: "body".to_owned(),
                needle: "οδυσσευς_target".as_bytes().to_vec(),
                case_insensitive: true,
            }],
            ..ScanRequest::default()
        })
        .unwrap();
    let got = concat_scanned_batches(&schema, &out);
    assert_eq!(got.num_rows(), 1);
    assert_eq!(
        got.column(0).unwrap().get_scalar(0).unwrap(),
        ScalarValue::I64(4)
    );
}

#[test]
fn m20_m22_empty_file_validation_id_lookup_date_decimal_and_dict_has_nan() {
    let schema = Arc::new(
        Schema::try_new(vec![
            Field::new("d", DataType::Date, false),
            Field::new(
                "dec",
                DataType::Decimal128 {
                    precision: 18,
                    scale: 4,
                },
                false,
            ),
            Field::new("score", DataType::F64, false),
        ])
        .unwrap(),
    );

    // 1. Empty file must still validate unknown filter/id_lookup/substring/map_keys columns.
    let mut empty_bytes = Vec::new();
    let w = StatamWriter::new(
        std::io::Cursor::new(&mut empty_bytes),
        Arc::clone(&schema),
        WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast),
        None,
    )
    .unwrap();
    w.finish().unwrap();
    let empty_reader = StatamReader::open(Arc::new(SliceReader::new(empty_bytes)), None).unwrap();
    assert!(matches!(
        empty_reader.scan(&ScanRequest {
            predicates: vec![Predicate {
                column: "does_not_exist".to_owned(),
                map_key: None,
                pred: ZonePred::IsNotNull,
            }],
            ..ScanRequest::default()
        }),
        Err(StatamError::SchemaMismatch { .. })
    ));
    assert!(matches!(
        empty_reader.scan(&ScanRequest {
            id_lookups: vec![IdLookup::eq("score", 1.0f64.to_le_bytes())],
            ..ScanRequest::default()
        }),
        Err(StatamError::SchemaMismatch { .. })
    ));

    // 2. Non-empty file: IdLookup succeeds on Date (4B LE) and Decimal128 (16B LE).
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(FlatData::Date(vec![19_000, 19_001, 19_002]), None).unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Decimal128 {
                        values: vec![100_000, 200_000, 300_000],
                        precision: 18,
                        scale: 4,
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(FlatData::F64(vec![1.0, f64::NAN, 2.0]), None).unwrap(),
            ),
        ],
        3,
    )
    .unwrap();
    let mut file_bytes = Vec::new();
    let mut w = StatamWriter::new(
        std::io::Cursor::new(&mut file_bytes),
        Arc::clone(&schema),
        WriteOptions::for_table(TableKind::Generic, WriteProfile::L1Compact),
        None,
    )
    .unwrap();
    w.write_batch(&batch).unwrap();
    w.finish().unwrap();
    let reader = StatamReader::open(Arc::new(SliceReader::new(file_bytes)), None).unwrap();

    let date_hits = reader
        .scan(&ScanRequest {
            id_lookups: vec![IdLookup::eq("d", 19_001i32.to_le_bytes())],
            ..ScanRequest::default()
        })
        .unwrap();
    assert_eq!(concat_scanned_batches(&schema, &date_hits).num_rows(), 1);

    let dec_hits = reader
        .scan(&ScanRequest {
            id_lookups: vec![IdLookup::eq("dec", 300_000i128.to_le_bytes())],
            ..ScanRequest::default()
        })
        .unwrap();
    assert_eq!(concat_scanned_batches(&schema, &dec_hits).num_rows(), 1);

    // 3. m20: eval_dict_pred propagates has_nan so F64 DictColumn containing NaN evaluates accurately.
    let dict_vals = FlatColumn::try_new(FlatData::F64(vec![1.0, f64::NAN, 5.0]), None).unwrap();
    let dict_col = cetra_column::DictColumn::try_new(dict_vals, vec![0, 1, 2, 1], None).unwrap();
    let bm_eq_nan =
        eval_dict_pred(&dict_col, &ZonePred::Eq(ZoneValue::F64(f64::NAN)), true).unwrap();
    assert_eq!(bm_eq_nan.iter_ones().collect::<Vec<_>>(), vec![1, 3]);
    let bm_lt_2 = eval_dict_pred(&dict_col, &ZonePred::Lt(ZoneValue::F64(2.0)), true).unwrap();
    assert_eq!(bm_lt_2.iter_ones().collect::<Vec<_>>(), vec![0]);

    // 4. m20: FileDict DataType mismatch is rejected on hot_dict / open.
    let str_schema =
        Arc::new(Schema::try_new(vec![Field::new("model", DataType::Utf8, false)]).unwrap());
    let mut offs = vec![0u32];
    let mut data = Vec::new();
    let models = ["gpt-4o", "claude-3", "gemini-2"];
    for i in 0..64usize {
        data.extend_from_slice(models[i % models.len()].as_bytes());
        offs.push(data.len() as u32);
    }
    let str_batch = Batch::try_new_all(
        Arc::clone(&str_schema),
        vec![Column::Flat(
            FlatColumn::try_new(FlatData::Utf8(Utf8Data::try_new(offs, data).unwrap()), None)
                .unwrap(),
        )],
        64,
    )
    .unwrap();
    let mut dict_file = Vec::new();
    let mut w2 = StatamWriter::new(
        std::io::Cursor::new(&mut dict_file),
        Arc::clone(&str_schema),
        WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact),
        None,
    )
    .unwrap();
    w2.write_batch(&str_batch).unwrap();
    w2.finish().unwrap();

    let mut meta = StatamFileMeta::read_from(&SliceReader::new(dict_file), None).unwrap();
    assert!(meta.hot_dict(0).unwrap().is_some());
    // Replace the hot dict bytes with a validly encoded Binary FileDict for a Utf8 column.
    let bin_dict = FileDict::from_flat(
        FlatColumn::try_new(
            FlatData::Binary(BinaryData::try_from_slices([b"aaa".as_slice(), b"bbb"]).unwrap()),
            None,
        )
        .unwrap(),
        true,
    )
    .unwrap();
    let bin_dict_bytes = bin_dict.encode(Codec::None, 3).unwrap();
    meta.col_dir[0].dict_offset = 0;
    meta.col_dir[0].dict_len = bin_dict_bytes.len() as u32;
    meta.hot_dict_bytes = Bytes::from(bin_dict_bytes);
    assert!(matches!(
        meta.hot_dict(0),
        Err(StatamError::Corrupt {
            reason: "dictionary data type does not match column schema"
        })
    ));
}

#[test]
fn m2_runtime_scan_and_take_reject_block_entry_header_mismatch() {
    let schema = Schema::try_new_arc(vec![Field::new("val", DataType::I64, false)]).unwrap();
    let batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![Column::Flat(
            FlatColumn::try_new(FlatData::I64((0..64i64).collect()), None).unwrap(),
        )],
        64,
    )
    .unwrap();
    let mut file_bytes = Vec::new();
    let mut w = StatamWriter::new(
        std::io::Cursor::new(&mut file_bytes),
        Arc::clone(&schema),
        WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast),
        None,
    )
    .unwrap();
    w.write_batch(&batch).unwrap();
    w.finish().unwrap();

    let reader = StatamReader::open(Arc::new(SliceReader::new(file_bytes.clone())), None).unwrap();
    let blk = reader.meta().block_dir[0];
    assert!(blk.stored_len > 0);
    let blk_start = blk.offset as usize;
    let blk_end = blk_start + BLOCK_FRAME_HEADER_LEN + blk.stored_len as usize;

    // 1. Flip SB header byte without updating CRC -> ChecksumMismatch during scan & take_rows.
    let mut bad_crc = file_bytes.clone();
    bad_crc[blk_start + 2] = if blk.encoding == Encoding::Plain {
        Encoding::Rle as u8
    } else {
        Encoding::Plain as u8
    };
    let r_crc = StatamReader::open(Arc::new(SliceReader::new(bad_crc)), None).unwrap();
    assert!(matches!(
        r_crc.scan(&ScanRequest::default()),
        Err(StatamError::ChecksumMismatch { .. })
    ));
    assert!(matches!(
        r_crc.take_rows(&[0], None, 1),
        Err(StatamError::ChecksumMismatch { .. })
    ));

    // 2. Recompute SB header-inclusive CRC with wrong encoding -> read_framed_block_checked_cow
    //    rejects mismatch against BlockEntry as Corrupt during scan & take_rows.
    let mut bad_enc = file_bytes;
    bad_enc[blk_start + 2] = if blk.encoding == Encoding::Plain {
        Encoding::Rle as u8
    } else {
        Encoding::Plain as u8
    };
    let recomputed = crate::format::block_frame_crc(
        &bad_enc[blk_start..blk_start + 12],
        &bad_enc[blk_start + BLOCK_FRAME_HEADER_LEN..blk_end],
    );
    bad_enc[blk_start + 12..blk_start + 16].copy_from_slice(&recomputed.to_le_bytes());
    let r_enc = StatamReader::open(Arc::new(SliceReader::new(bad_enc)), None).unwrap();
    assert!(matches!(
        r_enc.scan(&ScanRequest::default()),
        Err(StatamError::Corrupt { .. })
    ));
    assert!(matches!(
        r_enc.take_rows(&[0], None, 1),
        Err(StatamError::Corrupt { .. })
    ));
}

#[test]
fn m12_sort_desc_hint_unsorted_safety_point_head_and_scan_coalescing() {
    // 1. Unsorted file with sort_desc_hint + limit must NOT silently drop newest rows.
    let schema = Schema::try_new_arc(vec![
        Field::new("ts", DataType::timestamp_utc(), false),
        Field::new("tag", DataType::I64, false),
        Field::new("body", DataType::Utf8, false),
    ])
    .unwrap();

    let unsorted_ts = vec![100i64, 200, 300, 400, 50, 150, 999, 250];
    let unsorted_tags: Vec<i64> = (0..8).collect();
    let unsorted_bodies: Vec<String> = (0..8).map(|i| format!("row-{i}")).collect();
    let unsorted_batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: unsorted_ts,
                        utc: true,
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(FlatColumn::try_new(FlatData::I64(unsorted_tags), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(
                            unsorted_bodies
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
        8,
    )
    .unwrap();

    let mut unsorted_opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast);
    unsorted_opts.sort_cols = vec![0]; // Declared sorted, but actual MB zone maps are non-monotonic!
    unsorted_opts.mini_block_size = 4;
    unsorted_opts.row_group_size = 8;
    let mut unsorted_bytes = Vec::new();
    let mut wu = StatamWriter::new(
        std::io::Cursor::new(&mut unsorted_bytes),
        Arc::clone(&schema),
        unsorted_opts,
        None,
    )
    .unwrap();
    wu.write_batch(&unsorted_batch).unwrap();
    wu.finish().unwrap();

    let unsorted_reader =
        StatamReader::open(Arc::new(SliceReader::new(unsorted_bytes)), None).unwrap();
    assert!(!unsorted_reader.is_sort_col0_mb_monotonic());
    let out_unsorted = unsorted_reader
        .scan(&ScanRequest {
            sort_desc_hint: true,
            limit: Some(2),
            coalesce_mode: CoalesceMode::Scan,
            ..ScanRequest::default()
        })
        .unwrap();
    let merged_unsorted = concat_scanned_batches(&schema, &out_unsorted);
    assert_eq!(merged_unsorted.num_rows(), 8);
    let got_ts_col = merged_unsorted
        .column(0)
        .unwrap()
        .materialize(None)
        .unwrap();
    let FlatData::TimestampNs { values: got_ts, .. } = got_ts_col.data() else {
        panic!("expected TimestampNs");
    };
    assert!(got_ts.contains(&999));

    // 2. Forward CoalesceMode::Point + limit returns exact head rows.
    let out_point_head = unsorted_reader
        .scan(&ScanRequest {
            sort_desc_hint: false,
            limit: Some(3),
            coalesce_mode: CoalesceMode::Point,
            ..ScanRequest::default()
        })
        .unwrap();
    let merged_head = concat_scanned_batches(&schema, &out_point_head);
    assert_eq!(merged_head.num_rows(), 3);
    let head_ts_col = merged_head.column(0).unwrap().materialize(None).unwrap();
    let FlatData::TimestampNs {
        values: head_ts, ..
    } = head_ts_col.data()
    else {
        panic!("expected TimestampNs");
    };
    assert_eq!(head_ts.as_slice(), &[100i64, 200, 300]);

    // 3. Sorted 2-RG multi-MB file with external .stpk payload + CoalesceMode::Scan + desc_limit:
    //    - Uses 1 Phase-1 GET wave per RG (not serial per-MB waves).
    //    - Trims the final active MB mask to its tail BEFORE resolving .stpk payload chunks.
    let n = 64usize; // 2 RGs * 32 rows = 4 MBs of 8 rows per RG.
    let sorted_ts: Vec<i64> = (0..n as i64).map(|i| 10_000 + i * 10).collect();
    let sorted_tags: Vec<i64> = (0..n as i64).map(|i| i % 2).collect(); // even rows match tag == 0
    let sorted_bodies: Vec<String> = (0..n)
        .map(|i| format!("payload-row-{i:03}-{}", "X".repeat(400)))
        .collect();
    let sorted_batch = Batch::try_new_all(
        Arc::clone(&schema),
        vec![
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: sorted_ts,
                        utc: true,
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(FlatColumn::try_new(FlatData::I64(sorted_tags), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(
                            sorted_bodies.iter().map(String::as_str).collect::<Vec<_>>(),
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

    let pack_id = [0x99u8; 16];
    let mut pb = ChunkPackBuilder::new(
        pack_id,
        PackWriteOptions {
            target_frame_raw_bytes: 512, // Each chunk gets its own small frame so S3 pack bytes reflect exact chunks fetched!
            ..test_pack_opts()
        },
    );
    let mut sorted_opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L1Compact);
    sorted_opts.sort_cols = vec![0];
    sorted_opts.payload_cols = vec!["body".to_owned()];
    sorted_opts.pack_id = Some(pack_id);
    sorted_opts.row_group_size = 32;
    sorted_opts.mini_block_size = 8;

    let mut sorted_bytes = Vec::new();
    {
        let mut ws = StatamWriter::new(
            std::io::Cursor::new(&mut sorted_bytes),
            Arc::clone(&schema),
            sorted_opts,
            Some(&mut pb),
        )
        .unwrap();
        ws.write_batch(&sorted_batch).unwrap();
        ws.finish().unwrap();
    }
    let mut pack_bytes = Vec::new();
    pb.finish(&mut pack_bytes).unwrap();

    struct CountingRangeReader {
        inner: SliceReader,
        waves: std::sync::atomic::AtomicUsize,
        ranges_fetched: std::sync::atomic::AtomicUsize,
    }
    impl CountingRangeReader {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                inner: SliceReader::new(bytes),
                waves: std::sync::atomic::AtomicUsize::new(0),
                ranges_fetched: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn reset(&self) {
            self.waves.store(0, std::sync::atomic::Ordering::Relaxed);
            self.ranges_fetched
                .store(0, std::sync::atomic::Ordering::Relaxed);
        }
        fn waves(&self) -> usize {
            self.waves.load(std::sync::atomic::Ordering::Relaxed)
        }
        fn ranges_fetched(&self) -> usize {
            self.ranges_fetched
                .load(std::sync::atomic::Ordering::Relaxed)
        }
    }
    impl RangeReader for CountingRangeReader {
        fn file_len(&self) -> u64 {
            self.inner.file_len()
        }
        fn read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError> {
            self.waves
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.ranges_fetched
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.read_range(range)
        }
        fn read_ranges(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, StatamError> {
            if !ranges.is_empty() {
                self.waves
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.ranges_fetched
                    .fetch_add(ranges.len(), std::sync::atomic::Ordering::Relaxed);
            }
            self.inner.read_ranges(ranges)
        }
    }

    let s3_data = Arc::new(CountingRangeReader::new(sorted_bytes));
    let s3_pack = Arc::new(CountingRangeReader::new(pack_bytes));
    let pack_reader = Arc::new(ChunkPackReader::open(s3_pack.clone()).unwrap());
    let mut resolver = InMemoryPackResolver::new();
    resolver.insert(pack_id, pack_reader);

    let sorted_reader = StatamReader::open(s3_data.clone(), Some(Arc::new(resolver))).unwrap();
    assert!(sorted_reader.is_sort_col0_mb_monotonic());
    assert_eq!(sorted_reader.meta().rg_dir.len(), 2);

    // Filtered Scan + desc_limit=3: matches even rows in RG 1 (rows 62, 60, 58) — all inside MB 3!
    // With 1 Phase-1 GET wave + 1 Phase-2 GET wave in RG 1 (2 waves total, NOT 4+ serial MB waves),
    // and pack reads ONLY 3 frames (not all 4 even rows in MB 3)!
    s3_data.reset();
    s3_pack.reset();
    let desc_filtered = sorted_reader
        .scan(&ScanRequest {
            predicates: vec![Predicate::new("tag", ZonePred::Eq(ZoneValue::I64(0)))],
            sort_desc_hint: true,
            limit: Some(3),
            coalesce_mode: CoalesceMode::Scan,
            ..ScanRequest::default()
        })
        .unwrap();
    let data_waves = s3_data.waves();
    let pack_ranges_filtered = s3_pack.ranges_fetched();
    assert!(
        data_waves <= 2,
        "expected <= 2 coalesced RG waves (Phase-1 + Phase-2), got {data_waves}"
    );
    // Each chunk is in its own 512B frame -> 3 trimmed tail rows fetch <= 3 ranges (far fewer than 32 chunks in RG 1).
    assert!(
        pack_ranges_filtered <= 3,
        "expected payload fetches only for the 3 trimmed tail rows, got {pack_ranges_filtered} ranges"
    );

    let merged_desc = concat_scanned_batches(&schema, &desc_filtered);
    assert_eq!(merged_desc.num_rows(), 3);
    let d_ts_col = merged_desc.column(0).unwrap().materialize(None).unwrap();
    let FlatData::TimestampNs { values: d_ts, .. } = d_ts_col.data() else {
        panic!("expected TimestampNs");
    };
    let mut d_ts_sorted = d_ts.clone();
    d_ts_sorted.sort_unstable();
    assert_eq!(d_ts_sorted, vec![10_000 + 580, 10_000 + 600, 10_000 + 620]);

    // Unfiltered Scan + desc_limit=10 spans MB 3 (8 rows) + tail 2 rows of MB 2 (rows 54, 55).
    let desc_unfiltered = sorted_reader
        .scan(&ScanRequest {
            sort_desc_hint: true,
            limit: Some(10),
            coalesce_mode: CoalesceMode::Scan,
            ..ScanRequest::default()
        })
        .unwrap();
    let merged_unfilt = concat_scanned_batches(&schema, &desc_unfiltered);
    assert_eq!(merged_unfilt.num_rows(), 10);
    let u_ts_col = merged_unfilt.column(0).unwrap().materialize(None).unwrap();
    let FlatData::TimestampNs { values: u_ts, .. } = u_ts_col.data() else {
        panic!("expected TimestampNs");
    };
    let mut u_ts_sorted = u_ts.clone();
    u_ts_sorted.sort_unstable();
    let expected_tail10: Vec<i64> = (54..64).map(|i| 10_000 + i * 10).collect();
    assert_eq!(u_ts_sorted, expected_tail10);
}
