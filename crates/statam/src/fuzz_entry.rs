//! Fuzz entry point for `cetra-statam` `.statam`, `.stpk`, and paired/low-level decoders.
//! Must never panic, unwrap, or OOM on arbitrary untrusted bytes.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use cetra_column::DataType;

use crate::codec::dict::FileDict;
use crate::codec::{ZonePred, ZoneValue};
use crate::format::{MAX_BATCH_DECODED_BYTES, decode_schema};
use crate::gdpr::{GdprFileInput, gdpr_rewrite_day};
use crate::io::{RangeReader, SliceReader};
use crate::payload::chunker::chunk_payload;
use crate::payload::pack::ChunkPackReader;
use crate::payload::search::search_pack_chunks;
use crate::payload::{decode_payload_refs, decode_payload_refs_flat};
use crate::reader::{
    CoalesceMode, IdLookup, InMemoryPackResolver, PackResolver, Predicate, ScanRequest,
    StatamFileMeta, StatamReader, SubstringQuery, TimeRange,
};
use crate::simd::bloom::Sbbf;

const MAX_FUZZ_PACK_CHUNKS_TO_READ: usize = 32;
const MAX_FUZZ_PACK_FRAMES_TO_READ: usize = 4;

/// Exercises all untrusted `cetra-statam` parsing, scanning, search, GDPR rewrite,
/// and low-level decoder paths on arbitrary bytes `data`.
pub fn fuzz_statam_file(data: &[u8]) {
    // 1. Low-level decoders directly.
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
    let _ = decode_payload_refs_flat(payload_slice, row_count_hint, MAX_BATCH_DECODED_BYTES);
    let _ = decode_payload_refs_flat(data, 4, MAX_BATCH_DECODED_BYTES);

    let mut chunks = Vec::new();
    chunk_payload(data, &mut chunks);

    // 2. Standalone `.statam` path (default tail hint + small 64-byte tail hint).
    exercise_standalone_statam(data);

    // 3. Standalone `.stpk` pack path.
    exercise_standalone_pack(data);

    // 4. Paired `.statam` + `.stpk` path via length-prefixed split (or modulo fallback).
    if data.len() >= 8 {
        let len_a = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let len_b = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
        let exact_fit = 8usize
            .checked_add(len_a)
            .and_then(|end_a| end_a.checked_add(len_b))
            .is_some_and(|total| total <= data.len());
        if exact_fit {
            let statam_bytes = &data[8..8 + len_a];
            let pack_bytes = &data[8 + len_a..8 + len_a + len_b];
            exercise_paired(statam_bytes, pack_bytes);
        } else if data.len() > 8 {
            let body = &data[8..];
            let split = len_a % body.len();
            exercise_paired(&body[..split], &body[split..]);
        }
    }
}

fn exercise_standalone_statam(data: &[u8]) {
    let slice_reader = Arc::new(SliceReader::new(Bytes::copy_from_slice(data)));
    let _ = StatamFileMeta::read_from(slice_reader.as_ref(), Some(64));

    let Ok(reader) = StatamReader::open(slice_reader, None) else {
        return;
    };
    let _ = reader.verify_full();
    exercise_reader_queries(&reader);

    let inputs = [GdprFileInput {
        file_id: "fuzz-standalone".to_owned(),
        reader: Arc::new(SliceReader::new(Bytes::copy_from_slice(data))),
        write_options: None,
    }];
    let packs = HashMap::new();
    let _ = gdpr_rewrite_day(&inputs, &packs, &|_, row| row.is_multiple_of(2));
}

fn exercise_standalone_pack(data: &[u8]) {
    let slice_reader = Arc::new(SliceReader::new(Bytes::copy_from_slice(data)));
    let _ = ChunkPackReader::open_with_tail_hint(
        Arc::clone(&slice_reader) as Arc<dyn RangeReader>,
        Some(64),
    );

    let Ok(pack) = ChunkPackReader::open(slice_reader) else {
        return;
    };
    let _ = pack.read_edge_table();
    let _ = pack.read_edge_table_flat();
    let _ = pack.read_trigram_section();
    let _ = pack.read_hash_table();

    let n_frames = pack.toc().frames.len().min(MAX_FUZZ_PACK_FRAMES_TO_READ);
    for fid in 0..n_frames {
        let _ = pack.read_frame_raw(fid as u32);
        let _ = pack.read_frame_decompressed(fid as u32);
    }

    let n_chunks = pack.toc().chunks.len().min(MAX_FUZZ_PACK_CHUNKS_TO_READ);
    let ids: Vec<u32> = (0..n_chunks as u32).collect();
    let _ = pack.read_chunks(&ids);

    let _ = search_pack_chunks(&pack, b"err", false);
    let _ = search_pack_chunks(&pack, b"ERR", true);
    let _ = search_pack_chunks(&pack, "Alienación".as_bytes(), true);
    let _ = search_pack_chunks(&pack, &[b'a'; 72], false);
}

fn exercise_paired(statam_bytes: &[u8], pack_bytes: &[u8]) {
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
        Some(Arc::clone(&resolver_arc)),
    ) {
        let _ = statam.verify_full();
        exercise_reader_queries(&statam);
    }

    let inputs = [GdprFileInput {
        file_id: "fuzz-paired".to_owned(),
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

fn exercise_reader_queries(reader: &StatamReader) {
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
    let row_count = meta.postscript.row_count;
    let take_indices = [0u64, 1, 5, row_count.saturating_sub(1), row_count];
    let _ = reader.take_rows(&take_indices, None, 1);
    if !meta.schema().is_empty() {
        let _ = reader.take_rows(&take_indices, Some(&[0]), 1);
    }
    let fields = meta.schema().fields();

    let utf8_cols: Vec<&str> = fields
        .iter()
        .filter(|f| f.data_type == DataType::Utf8)
        .map(|f| f.name.as_str())
        .collect();
    if let Some(&first_utf8) = utf8_cols.first() {
        run_utf8_queries(reader, first_utf8);
        if let Some(&last_utf8) = utf8_cols.last()
            && last_utf8 != first_utf8
        {
            run_utf8_queries(reader, last_utf8);
        }
    }

    if let Some(fb16) = fields
        .iter()
        .find(|f| f.data_type == DataType::FixedBytes16)
    {
        let _ = reader.scan(&ScanRequest {
            id_lookups: vec![IdLookup::eq(&fb16.name, [0u8; 16])],
            coalesce_mode: CoalesceMode::Point,
            ..Default::default()
        });
    }

    if let Some(map_col) = fields.iter().find(|f| f.data_type == DataType::MapUtf8) {
        let key = meta
            .schema_meta
            .shredded_maps
            .first()
            .and_then(|sm| sm.keys.first())
            .map(|k| k.key.clone())
            .unwrap_or_else(|| "customer_id".to_owned());
        let _ = reader.scan(&ScanRequest {
            map_keys: vec![(map_col.name.clone(), key.clone())],
            ..Default::default()
        });
        let _ = reader.scan(&ScanRequest {
            predicates: vec![Predicate::map_key(
                &map_col.name,
                &key,
                ZonePred::Eq(ZoneValue::Bytes(b"cust_0".to_vec())),
            )],
            map_keys: vec![(map_col.name.clone(), key)],
            ..Default::default()
        });
    }

    if let Some(num_col) = fields.iter().find(|f| {
        matches!(
            f.data_type,
            DataType::I64 | DataType::TimestampNs { .. } | DataType::F64
        )
    }) {
        let pred = match num_col.data_type {
            DataType::F64 => ZonePred::Lt(ZoneValue::F64(100.0)),
            DataType::TimestampNs { .. } => ZonePred::Ge(ZoneValue::TimestampNs(0)),
            _ => ZonePred::Ge(ZoneValue::I64(0)),
        };
        let _ = reader.scan(&ScanRequest {
            predicates: vec![Predicate::new(&num_col.name, pred)],
            ..Default::default()
        });
    }
}

fn run_utf8_queries(reader: &StatamReader, col_name: &str) {
    let _ = reader.scan(&ScanRequest {
        substrings: vec![SubstringQuery::like(col_name, b"err")],
        ..Default::default()
    });
    let _ = reader.scan(&ScanRequest {
        substrings: vec![SubstringQuery::ilike(col_name, b"PROMPT")],
        ..Default::default()
    });
    let _ = reader.scan(&ScanRequest {
        substrings: vec![SubstringQuery::ilike(col_name, "Alienación".as_bytes())],
        ..Default::default()
    });
    let _ = reader.scan(&ScanRequest {
        substrings: vec![SubstringQuery::ilike(col_name, "\u{212A}".as_bytes())],
        ..Default::default()
    });
    let _ = reader.scan(&ScanRequest {
        id_lookups: vec![IdLookup::eq(col_name, b"user_0")],
        ..Default::default()
    });
    let _ = reader.scan(&ScanRequest {
        predicates: vec![Predicate::new(
            col_name,
            ZonePred::Eq(ZoneValue::Bytes(b"agent.step".to_vec())),
        )],
        ..Default::default()
    });
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};

    use cetra_column::{
        Batch, BinaryData, Bitmap, COST_DECIMAL_PRECISION, COST_DECIMAL_SCALE, Column, DataType,
        DictColumn, EventsData, Field, FlatColumn, FlatData, Interval, LinksData, MapData,
        RleColumn, ScalarValue, Schema, Selection, Utf8Data,
    };

    use crate::codec::WriteProfile;
    use crate::codec::dict::DictBuilder;
    use crate::format::{Codec, FileSchemaMeta, LocalityGroup, TableKind, encode_schema};
    use crate::payload::pack::{ChunkPackBuilder, PackWriteOptions};
    use crate::payload::{PayloadRefRow, encode_payload_refs};
    use crate::writer::{StatamWriter, WriteOptions};

    use super::*;

    fn corpus_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/statam_file")
    }

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

    fn spans_schema() -> Arc<Schema> {
        Schema::try_new_arc(vec![
            Field::new("trace_id", DataType::FixedBytes16, false),
            Field::new("span_id", DataType::FixedBytes8, false),
            Field::new("start_time", DataType::timestamp_utc(), false),
            Field::new("name", DataType::Utf8, false),
            Field::new("status_code", DataType::Utf8, false),
            Field::new("model", DataType::Utf8, false),
            Field::new("session_id", DataType::Utf8, false),
            Field::new("user_id", DataType::Utf8, false),
            Field::new("latency_ms", DataType::F64, false),
            Field::new("is_error", DataType::Bool, false),
            Field::new("attributes", DataType::MapUtf8, true),
            Field::new("input", DataType::Utf8, true),
            Field::new("output", DataType::Utf8, true),
        ])
        .unwrap()
    }

    fn spans_batch(schema: &Arc<Schema>, count: usize, include_long_payloads: bool) -> Batch {
        let mut trace_ids = Vec::with_capacity(count);
        let mut span_ids = Vec::with_capacity(count);
        let mut start_times = Vec::with_capacity(count);
        let mut names = Vec::with_capacity(count);
        let mut status_codes = Vec::with_capacity(count);
        let mut models = Vec::with_capacity(count);
        let mut session_ids = Vec::with_capacity(count);
        let mut user_ids = Vec::with_capacity(count);
        let mut latencies = Vec::with_capacity(count);
        let mut is_errors = Vec::with_capacity(count);
        let mut map_offsets = vec![0u32];
        let mut map_keys = Vec::new();
        let mut map_vals = Vec::new();
        let mut inputs = Vec::with_capacity(count);
        let mut outputs = Vec::with_capacity(count);

        for r in 0..count {
            let mut tid = [0u8; 16];
            tid[0..8].copy_from_slice(&((r / 4) as u64).to_be_bytes());
            tid[15] = 0xAA;
            trace_ids.push(tid);

            let mut sid = [0u8; 8];
            sid.copy_from_slice(&(r as u64).to_be_bytes());
            span_ids.push(sid);

            start_times.push(1_700_000_000_000_000_000i64 + (r as i64) * 1_000_000);
            names.push(if r.is_multiple_of(5) {
                "llm.error.retry"
            } else if r.is_multiple_of(2) {
                "tool.execute"
            } else {
                "agent.step"
            });
            status_codes.push(if r.is_multiple_of(7) { "ERROR" } else { "OK" });
            models.push(if r.is_multiple_of(2) {
                "gpt-4o"
            } else {
                "claude-3-5-sonnet"
            });
            session_ids.push(format!("sess_{}", r / 4));
            user_ids.push(format!("user_{}", r % 3));
            latencies.push(if r.is_multiple_of(11) {
                f64::NAN
            } else if r.is_multiple_of(13) {
                -0.0
            } else {
                (r as f64) * 1.25
            });
            is_errors.push(r.is_multiple_of(5));

            map_keys.push("customer_id".to_owned());
            map_vals.push(format!("cust_{}", r % 4));
            map_keys.push("env".to_owned());
            map_vals.push("production".to_owned());
            if r.is_multiple_of(10) {
                map_keys.push(format!("rare_{r}"));
                map_vals.push("v".to_owned());
            }
            map_offsets.push(map_keys.len() as u32);

            if include_long_payloads && !r.is_multiple_of(3) {
                let msg0 = format!(
                    "{{\"role\":\"system\",\"content\":\"shared system prompt {}\"}}",
                    "S".repeat(260)
                );
                let msg1 = format!(
                    "{{\"role\":\"user\",\"content\":\"turn {r} err prompt {}\"}}",
                    "U".repeat(260)
                );
                inputs.push(format!("[{msg0},{msg1}]"));
                outputs.push(format!("{{\"answer\":\"output {r} {}\"}}", "R".repeat(260)));
            } else {
                inputs.push(format!("short prompt {r} err"));
                outputs.push(format!("short reply {r}"));
            }
        }

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
                    FlatData::Utf8(Utf8Data::try_from_slices(names).unwrap()),
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
            Column::Flat(FlatColumn::try_new(FlatData::F64(latencies), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(FlatData::Bool(Bitmap::from_bools(&is_errors)), None).unwrap(),
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

    fn all_16_types_schema() -> Arc<Schema> {
        Schema::try_new_arc(vec![
            Field::new("null_col", DataType::Null, true),
            Field::new("bool_col", DataType::Bool, true),
            Field::new("i64_col", DataType::I64, true),
            Field::new("f64_col", DataType::F64, true),
            Field::new("dec_col", DataType::cost_decimal(), true),
            Field::new("ts_col", DataType::timestamp_utc(), true),
            Field::new("ts_naive_col", DataType::TimestampNs { utc: false }, true),
            Field::new("date_col", DataType::Date, true),
            Field::new("iv_col", DataType::Interval, true),
            Field::new("utf8_col", DataType::Utf8, true),
            Field::new("bin_col", DataType::Binary, true),
            Field::new("fb8_col", DataType::FixedBytes8, true),
            Field::new("fb16_col", DataType::FixedBytes16, true),
            Field::new("list_col", DataType::List(Box::new(DataType::I64)), true),
            Field::new("map_col", DataType::MapUtf8, true),
            Field::new("events_col", DataType::Events, true),
            Field::new("links_col", DataType::Links, true),
        ])
        .unwrap()
    }

    fn all_16_types_batch() -> Batch {
        let schema = all_16_types_schema();
        let n = 4usize;
        let nulls = Some(Bitmap::from_bools(&[true, false, true, true]));

        let c_null = Column::Flat(FlatColumn::try_new(FlatData::Null(n), None).unwrap());
        let c_bool = Column::Flat(
            FlatColumn::try_new(
                FlatData::Bool(Bitmap::from_bools(&[true, false, false, true])),
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_i64 = Column::Flat(
            FlatColumn::try_new(FlatData::I64(vec![10, 0, -5, 42]), nulls.clone()).unwrap(),
        );
        let c_f64 = Column::Flat(
            FlatColumn::try_new(
                FlatData::F64(vec![1.25, 0.0, f64::NAN, -0.0]),
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_dec = Column::Flat(
            FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vec![1_000_000_000_000_000_000, 0, -42, 999],
                    precision: COST_DECIMAL_PRECISION,
                    scale: COST_DECIMAL_SCALE,
                },
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_ts = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vec![1, 0, 2, 3],
                    utc: true,
                },
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_ts_naive = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vec![4, 0, 5, 6],
                    utc: false,
                },
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_date = Column::Flat(
            FlatColumn::try_new(FlatData::Date(vec![19_000, 0, -1, 20_000]), nulls.clone())
                .unwrap(),
        );
        let c_iv = Column::Flat(
            FlatColumn::try_new(
                FlatData::Interval(vec![
                    Interval {
                        months: 1,
                        days: 2,
                        nanos: 3,
                    },
                    Interval::default(),
                    Interval {
                        months: -1,
                        days: -2,
                        nanos: -3,
                    },
                    Interval {
                        months: 0,
                        days: 7,
                        nanos: 0,
                    },
                ]),
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_utf8 = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(["hello", "", "err-🚀", "world"]).unwrap(),
                ),
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_bin = Column::Flat(
            FlatColumn::try_new(
                FlatData::Binary(
                    BinaryData::try_from_slices([
                        &b"ab"[..],
                        &b""[..],
                        &b"\x00\xff"[..],
                        &b"z"[..],
                    ])
                    .unwrap(),
                ),
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_fb8 = Column::Flat(
            FlatColumn::try_new(
                FlatData::FixedBytes8(vec![[1; 8], [0; 8], [2; 8], [3; 8]]),
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_fb16 = Column::Flat(
            FlatColumn::try_new(
                FlatData::FixedBytes16(vec![[4; 16], [0; 16], [5; 16], [6; 16]]),
                nulls.clone(),
            )
            .unwrap(),
        );
        let list_items = FlatColumn::try_new(
            FlatData::I64(vec![1, 0, 3, 4]),
            Some(Bitmap::from_bools(&[true, false, true, true])),
        )
        .unwrap();
        let c_list = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::I64,
                    offsets: vec![0, 2, 2, 3, 4],
                    values: Box::new(Column::Flat(list_items)),
                },
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_map = Column::Flat(
            FlatColumn::try_new(
                FlatData::MapUtf8(Box::new(
                    MapData::try_new(
                        vec![0, 2, 2, 3, 3],
                        Utf8Data::try_from_slices(["customer_id", "k2", "customer_id"]).unwrap(),
                        Utf8Data::try_from_slices(["cust_0", "", "cust_1"]).unwrap(),
                        Some(Bitmap::from_bools(&[true, false, true])),
                    )
                    .unwrap(),
                )),
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_events = Column::Flat(
            FlatColumn::try_new(
                FlatData::Events(Box::new(
                    EventsData::try_new(
                        vec![0, 1, 1, 2, 2],
                        vec![100, 200],
                        Utf8Data::try_from_slices(["ev1", "ev2"]).unwrap(),
                        Utf8Data::try_from_slices(["{}", ""]).unwrap(),
                        Some(Bitmap::from_bools(&[true, false])),
                    )
                    .unwrap(),
                )),
                nulls.clone(),
            )
            .unwrap(),
        );
        let c_links = Column::Flat(
            FlatColumn::try_new(
                FlatData::Links(Box::new(
                    LinksData::try_new(
                        vec![0, 1, 1, 2, 2],
                        vec![[9; 16], [8; 16]],
                        vec![[7; 8], [6; 8]],
                        Utf8Data::try_from_slices(["", "{\"a\":1}"]).unwrap(),
                        Some(Bitmap::from_bools(&[false, true])),
                    )
                    .unwrap(),
                )),
                nulls,
            )
            .unwrap(),
        );

        Batch::try_new_all(
            schema,
            vec![
                c_null, c_bool, c_i64, c_f64, c_dec, c_ts, c_ts_naive, c_date, c_iv, c_utf8, c_bin,
                c_fb8, c_fb16, c_list, c_map, c_events, c_links,
            ],
            n,
        )
        .unwrap()
    }

    fn write_statam_bytes(schema: &Arc<Schema>, batches: &[&Batch], opts: WriteOptions) -> Vec<u8> {
        let mut out = Vec::new();
        let mut w = StatamWriter::new(Arc::clone(schema), opts, None).unwrap();
        for b in batches {
            w.write_batch(b).unwrap();
        }
        w.finish(&mut out).unwrap();
        out
    }

    fn encode_paired_seed(statam_bytes: &[u8], pack_bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + statam_bytes.len() + pack_bytes.len());
        out.extend_from_slice(&(statam_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(pack_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(statam_bytes);
        out.extend_from_slice(pack_bytes);
        out
    }

    /// Generates ~18 diverse valid and edge-case seed files into `dir`.
    pub(crate) fn generate_seed_corpus(dir: &Path) {
        std::fs::create_dir_all(dir).expect("create seed corpus dir");
        let write = |name: &str, bytes: &[u8]| {
            std::fs::write(dir.join(name), bytes).expect("write seed file");
        };

        let sp_schema = spans_schema();
        let sp_batch_short = spans_batch(&sp_schema, 16, false);
        let sp_batch_long = spans_batch(&sp_schema, 16, true);

        // 01: L0Fast spans with local L0 payloads & multi-RG.
        let mut l0_opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
        l0_opts.row_group_size = 8;
        l0_opts.mini_block_size = 4;
        let l0_spans = write_statam_bytes(&sp_schema, &[&sp_batch_long], l0_opts);
        write("seed-01-l0-spans", &l0_spans);

        // 02: L1Compact spans (inline payloads, narrow trigram on `name`, sorted dicts).
        let mut l1_opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        l1_opts.sort_cols = vec![2, 0];
        l1_opts.row_group_size = 8;
        l1_opts.mini_block_size = 4;
        let l1_spans = write_statam_bytes(&sp_schema, &[&sp_batch_short], l1_opts);
        write("seed-02-l1-spans", &l1_spans);

        // 03: L1Compact traces.
        let traces_schema = Schema::try_new_arc(vec![
            Field::new("trace_id", DataType::FixedBytes16, false),
            Field::new("session_id", DataType::Utf8, true),
            Field::new("start_time", DataType::timestamp_utc(), false),
            Field::new("duration_ns", DataType::I64, false),
            Field::new("total_cost", DataType::cost_decimal(), true),
        ])
        .unwrap();
        let traces_batch = Batch::try_new_all(
            Arc::clone(&traces_schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::FixedBytes16((0..8u8).map(|i| [i; 16]).collect()),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices([
                                "s0", "s1", "s0", "s2", "s0", "s1", "s3", "s0",
                            ])
                            .unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: (0..8).map(|i| 1_700_000_000 + i * 10).collect(),
                            utc: true,
                        },
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(FlatData::I64(vec![10, 20, 30, 40, 50, 60, 70, 80]), None)
                        .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Decimal128 {
                            values: vec![100, 200, 300, 400, 500, 600, 700, 800],
                            precision: COST_DECIMAL_PRECISION,
                            scale: COST_DECIMAL_SCALE,
                        },
                        None,
                    )
                    .unwrap(),
                ),
            ],
            8,
        )
        .unwrap();
        let l1_traces = write_statam_bytes(
            &traces_schema,
            &[&traces_batch],
            WriteOptions::for_table(TableKind::Traces, WriteProfile::L1Compact),
        );
        write("seed-03-l1-traces", &l1_traces);

        // 04: L0Fast scores with NaN / -0.0 / Inf.
        let scores_schema = Schema::try_new_arc(vec![
            Field::new("trace_id", DataType::FixedBytes16, false),
            Field::new("score_id", DataType::FixedBytes16, false),
            Field::new("timestamp", DataType::timestamp_utc(), false),
            Field::new("name", DataType::Utf8, false),
            Field::new("value", DataType::F64, false),
            Field::new("date", DataType::Date, false),
        ])
        .unwrap();
        let scores_batch = Batch::try_new_all(
            Arc::clone(&scores_schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::FixedBytes16(vec![[1; 16], [2; 16], [3; 16], [4; 16]]),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::FixedBytes16(vec![[10; 16], [20; 16], [30; 16], [40; 16]]),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: vec![100, 200, 300, 400],
                            utc: true,
                        },
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(["acc", "err_rate", "acc", "lat"]).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::F64(vec![0.95, f64::NAN, -0.0, f64::INFINITY]),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(FlatData::Date(vec![19_800, 19_801, 19_802, 19_803]), None)
                        .unwrap(),
                ),
            ],
            4,
        )
        .unwrap();
        let l0_scores = write_statam_bytes(
            &scores_schema,
            &[&scores_batch],
            WriteOptions::for_table(TableKind::Scores, WriteProfile::L0Fast),
        );
        write("seed-04-l0-scores", &l0_scores);

        // 05 & 06: All 16 DataTypes in L0Fast and L1Compact.
        let all16_schema = all_16_types_schema();
        let all16_batch = all_16_types_batch();
        write(
            "seed-05-all16-l0",
            &write_statam_bytes(
                &all16_schema,
                &[&all16_batch],
                WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast),
            ),
        );
        write(
            "seed-06-all16-l1",
            &write_statam_bytes(
                &all16_schema,
                &[&all16_batch],
                WriteOptions::for_table(TableKind::Generic, WriteProfile::L1Compact),
            ),
        );

        // 07: Empty 0-row `.statam` file.
        let empty_batch = Batch::empty(Arc::clone(&all16_schema));
        write(
            "seed-07-empty",
            &write_statam_bytes(
                &all16_schema,
                &[&empty_batch],
                WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast),
            ),
        );

        // 08: Encodings + nested list + shredded map edge cases (null values, duplicate keys, Dict/Rle/Const).
        let enc_schema = Schema::try_new_arc(vec![
            Field::new("const_str", DataType::Utf8, false),
            Field::new("dict_str", DataType::Utf8, true),
            Field::new("rle_i64", DataType::I64, true),
            Field::new(
                "nested_list",
                DataType::List(Box::new(DataType::List(Box::new(DataType::Utf8)))),
                true,
            ),
            Field::new("attrs", DataType::MapUtf8, true),
        ])
        .unwrap();
        let inner_utf8 = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["a", "b", "c", "d"]).unwrap()),
            None,
        )
        .unwrap();
        let l1_list = FlatColumn::try_new(
            FlatData::List {
                item_type: DataType::Utf8,
                offsets: vec![0, 2, 3, 4],
                values: Box::new(Column::Flat(inner_utf8)),
            },
            None,
        )
        .unwrap();
        let l2_list = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::List(Box::new(DataType::Utf8)),
                    offsets: vec![0, 1, 2, 3],
                    values: Box::new(Column::Flat(l1_list)),
                },
                None,
            )
            .unwrap(),
        );
        let enc_batch = Batch::try_new(
            Arc::clone(&enc_schema),
            vec![
                Column::try_new_const(ScalarValue::Utf8("constant-val".to_owned()), 3).unwrap(),
                Column::Dict(
                    DictColumn::try_new(
                        FlatColumn::try_new(
                            FlatData::Utf8(Utf8Data::try_from_slices(["alpha", "beta"]).unwrap()),
                            None,
                        )
                        .unwrap(),
                        vec![0, 1, 0],
                        None,
                    )
                    .unwrap(),
                ),
                Column::Rle(
                    RleColumn::try_new(
                        FlatColumn::try_new(FlatData::I64(vec![11, 22]), None).unwrap(),
                        vec![2, 3],
                    )
                    .unwrap(),
                ),
                l2_list,
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::MapUtf8(Box::new(
                            MapData::try_new(
                                vec![0, 3, 5, 7],
                                Utf8Data::try_from_slices([
                                    "customer_id",
                                    "customer_id",
                                    "null_key",
                                    "customer_id",
                                    "rare_a",
                                    "customer_id",
                                    "null_key",
                                ])
                                .unwrap(),
                                Utf8Data::try_from_slices([
                                    "cust_0", "cust_dup", "", "cust_1", "v", "cust_2", "",
                                ])
                                .unwrap(),
                                Some(Bitmap::from_bools(&[
                                    true, true, false, true, true, true, false,
                                ])),
                            )
                            .unwrap(),
                        )),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            3,
            Selection::Mask(Bitmap::from_bools(&[true, false, true])),
        )
        .unwrap();
        write(
            "seed-08-shredded-nested",
            &write_statam_bytes(
                &enc_schema,
                &[&enc_batch],
                WriteOptions::for_table(TableKind::Generic, WriteProfile::L1Compact),
            ),
        );

        // 09 & 10: Standalone `.stpk` packs (single-frame + multi-frame).
        let pack_id_a = [0x31u8; 16];
        let mut pb_a = ChunkPackBuilder::new(pack_id_a, PackWriteOptions::default());
        for i in 0..8usize {
            let chunk = format!(
                "{{\"turn\":{i},\"msg\":\"payload err text {}\"}}",
                "A".repeat(320)
            );
            pb_a.intern_chunk(chunk.as_bytes()).unwrap();
        }
        // Duplicate chunk to exercise BLAKE3 dedup.
        let dup = format!(
            "{{\"turn\":0,\"msg\":\"payload err text {}\"}}",
            "A".repeat(320)
        );
        pb_a.intern_chunk(dup.as_bytes()).unwrap();
        let mut pack_a_bytes = Vec::new();
        pb_a.finish(&mut pack_a_bytes).unwrap();
        write("seed-09-stpk-single", &pack_a_bytes);

        let pack_id_b = [0x32u8; 16];
        let multi_opts = PackWriteOptions {
            target_frame_raw_bytes: 2048,
            ..Default::default()
        };
        let mut pb_b = ChunkPackBuilder::new(pack_id_b, multi_opts);
        for i in 0..10usize {
            let chunk = format!("frame-{i}-boundary-{}-end", "XY".repeat(200));
            pb_b.intern_chunk(chunk.as_bytes()).unwrap();
        }
        let mut pack_b_bytes = Vec::new();
        pb_b.finish(&mut pack_b_bytes).unwrap();
        write("seed-10-stpk-multi-frame", &pack_b_bytes);

        // 11: Paired L1Compact spans `.statam` + `.stpk`.
        let paired_pack_id = [0x41u8; 16];
        let mut paired_opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        paired_opts.pack_id = Some(paired_pack_id);
        paired_opts.row_group_size = 8;
        paired_opts.mini_block_size = 4;
        let mut paired_pb = ChunkPackBuilder::new(paired_pack_id, PackWriteOptions::default());
        let mut paired_statam = Vec::new();
        {
            let mut w =
                StatamWriter::new(Arc::clone(&sp_schema), paired_opts, Some(&mut paired_pb))
                    .unwrap();
            w.write_batch(&sp_batch_long).unwrap();
            w.finish(&mut paired_statam).unwrap();
        }
        let mut paired_pack = Vec::new();
        paired_pb.finish(&mut paired_pack).unwrap();
        write(
            "seed-11-paired-l1-spans",
            &encode_paired_seed(&paired_statam, &paired_pack),
        );

        // 12: Paired multi-turn conversation tail-sharing + mixed live/dead GDPR frames.
        let mt_schema = Schema::try_new_arc(vec![
            Field::new("user_id", DataType::Utf8, false),
            Field::new("input", DataType::Utf8, true),
        ])
        .unwrap();
        let turn0 = format!(
            "{{\"role\":\"system\",\"content\":\"{}\"}}",
            "S".repeat(320)
        );
        let turn1 = format!(
            "{{\"role\":\"user\",\"content\":\"hello {}\"}}",
            "U".repeat(320)
        );
        let turn2 = format!(
            "{{\"role\":\"assistant\",\"content\":\"reply err {}\"}}",
            "A".repeat(320)
        );
        let turn3 = format!(
            "{{\"role\":\"user\",\"content\":\"follow {}\"}}",
            "F".repeat(320)
        );
        let mt_inputs = [
            format!("[{turn0}]"),
            format!("[{turn0},{turn1}]"),
            format!("[{turn0},{turn1},{turn2}]"),
            format!("[{turn0},{turn1},{turn2},{turn3}]"),
        ];
        let mt_batch = Batch::try_new_all(
            Arc::clone(&mt_schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(["u0", "u1", "u0", "u1"]).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(
                                mt_inputs.iter().map(String::as_str).collect::<Vec<_>>(),
                            )
                            .unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            4,
        )
        .unwrap();
        let mt_pack_id = [0x42u8; 16];
        let mut mt_opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        mt_opts.payload_cols = vec!["input".to_owned()];
        mt_opts.pack_id = Some(mt_pack_id);
        let mt_pack_opts = PackWriteOptions {
            target_frame_raw_bytes: 512,
            ..Default::default()
        };
        let mut mt_pb = ChunkPackBuilder::new(mt_pack_id, mt_pack_opts);
        let mut mt_statam = Vec::new();
        {
            let mut w =
                StatamWriter::new(Arc::clone(&mt_schema), mt_opts, Some(&mut mt_pb)).unwrap();
            w.write_batch(&mt_batch).unwrap();
            w.finish(&mut mt_statam).unwrap();
        }
        let mut mt_pack = Vec::new();
        mt_pb.finish(&mut mt_pack).unwrap();
        write(
            "seed-12-paired-multiturn",
            &encode_paired_seed(&mt_statam, &mt_pack),
        );

        // 13: Low-level SBBF (`BF01`).
        let mut sbbf = Sbbf::new_for_ndv(64, 0.01);
        sbbf.insert_bytes(b"trace-1");
        sbbf.insert_bytes(b"trace-2");
        write("seed-13-lowlevel-sbbf", &sbbf.to_bytes());

        // 14: Low-level FileDict (`DT01`).
        let dict_col = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["alpha", "beta", "gamma", "alpha"]).unwrap()),
            None,
        )
        .unwrap();
        let (fdict, _) = DictBuilder::l1_compact().build(&dict_col).unwrap().unwrap();
        write(
            "seed-14-lowlevel-dict",
            &fdict.encode(Codec::Lz4, 3).unwrap(),
        );

        // 15: Low-level FileSchemaMeta (`encode_schema`).
        let schema_meta = FileSchemaMeta {
            table_kind: TableKind::Generic,
            sort_cols: vec![2],
            pack_ids: vec![[0x55; 16]],
            schema: Arc::clone(&all16_schema),
            locality_groups: vec![LocalityGroup::Core; all16_schema.len()],
            shredded_maps: Vec::new(),
        };
        write(
            "seed-15-lowlevel-schema",
            &encode_schema(&schema_meta).unwrap(),
        );

        // 16: Low-level PayloadRefs (2-byte row count prefix + encoded PayloadRefRow stream).
        let rows = vec![
            PayloadRefRow::Null,
            PayloadRefRow::Inline(b"short-inline".to_vec()),
            PayloadRefRow::Chunks(vec![(0, 0), (0, 1)]),
            PayloadRefRow::Chunks(vec![(0, 0), (0, 1), (0, 2)]),
        ];
        let mut pref_bytes = (rows.len() as u16).to_le_bytes().to_vec();
        encode_payload_refs(&rows, &mut pref_bytes).unwrap();
        write("seed-16-lowlevel-payload-refs", &pref_bytes);

        // 17: Truncated `.statam` file.
        let trunc_len = l0_spans.len() / 2;
        write("seed-17-truncated-statam", &l0_spans[..trunc_len]);

        // 18: Corrupted CRC `.statam` file (bit flipped in Postscript).
        let mut corrupt = l1_spans;
        if let Some(last) = corrupt.last_mut() {
            *last ^= 0xFF;
        }
        write("seed-18-corrupt-crc-statam", &corrupt);
    }

    /// Writes the seed corpus for `statam_file` (`just fuzz-seed`).
    #[test]
    #[ignore]
    fn write_seed_corpus() {
        generate_seed_corpus(&corpus_dir());
    }

    #[test]
    fn replay_statam_file_corpus() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz");
        let corpus = sorted_files(&root.join("corpus/statam_file"));
        assert!(
            !corpus.is_empty(),
            "statam_file seed corpus must not be empty"
        );
        assert!(corpus.len() <= 300, "statam_file corpus exceeds 300 files");
        let total_bytes: u64 = corpus
            .iter()
            .map(|p| std::fs::metadata(p).unwrap().len())
            .sum();
        assert!(
            total_bytes < 512 * 1024,
            "statam_file seed corpus ({total_bytes} bytes) exceeds 512 KiB"
        );

        let regressions = sorted_files(&root.join("regressions/statam_file"));
        for path in corpus.into_iter().chain(regressions) {
            let data = std::fs::read(&path).expect("read statam_file corpus entry");
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                fuzz_statam_file(&data);
            }));
            if outcome.is_err() {
                panic!("statam_file: {} panicked", path.display());
            }
        }
    }
}
