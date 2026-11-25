//! Fuzz entry point for `cetra-column` binary framing (D-QE-7). Must never panic.
#![allow(clippy::expect_used, clippy::unwrap_used)] // harness setup, not input-dependent

/// Decodes arbitrary bytes as a single frame, a multi-frame stream, and CRC-repaired / wrapped
/// frames past the CRC gate to exercise inner LZ4, `BodyReader`, and `Reservation` invariants.
pub fn column_frame(data: &[u8]) {
    let _ = crate::frame::decode_batch(data, None, None);
    let mut offset = 0;
    while let crate::frame::FrameSlice::Frame { frame, next } =
        crate::frame::decode_next(data, offset)
    {
        let _ = crate::frame::decode_batch(&data[frame], None, None);
        assert!(next > offset, "framing must advance");
        offset = next;
    }

    if data.len() >= crate::frame::FRAME_HEADER_LEN {
        let mut repaired = data.to_vec();
        let hdr_payload_len =
            u32::from_le_bytes([repaired[12], repaired[13], repaired[14], repaired[15]]) as usize;
        let matches_len =
            crate::frame::FRAME_HEADER_LEN.checked_add(hdr_payload_len) == Some(repaired.len());
        let len_ok = if matches_len {
            true
        } else if let Ok(actual_len) =
            u32::try_from(repaired.len() - crate::frame::FRAME_HEADER_LEN)
        {
            repaired[0..4].copy_from_slice(&crate::frame::FRAME_MAGIC);
            repaired[4] = crate::frame::FRAME_VERSION;
            repaired[12..16].copy_from_slice(&actual_len.to_le_bytes());
            true
        } else {
            false
        };
        if len_ok {
            let payload = &repaired[crate::frame::FRAME_HEADER_LEN..];
            let crc = crc32c::crc32c_append(crc32c::crc32c(&repaired[0..16]), payload);
            repaired[16..20].copy_from_slice(&crc.to_le_bytes());

            let scope = crate::GlobalPool::standalone(2 * 1024 * 1024);
            let mut res = scope.register("fuzz_repaired", true);
            match crate::frame::decode_batch_with_limit(
                &repaired,
                None,
                Some(&mut res),
                1024 * 1024,
            ) {
                Ok(batch) => assert_eq!(res.size(), batch.memory_bytes()),
                Err(_) => assert_eq!(res.size(), 0),
            }
        }
    }

    if let Ok(len_u32) = u32::try_from(data.len()) {
        let mut wrapped = Vec::with_capacity(crate::frame::FRAME_HEADER_LEN + data.len());
        let mut hdr = [0u8; crate::frame::FRAME_HEADER_LEN];
        hdr[0..4].copy_from_slice(&crate::frame::FRAME_MAGIC);
        hdr[4] = crate::frame::FRAME_VERSION;
        hdr[5] = 0;
        hdr[6..8].copy_from_slice(&0u16.to_le_bytes());
        let len_bytes = len_u32.to_le_bytes();
        hdr[8..12].copy_from_slice(&len_bytes);
        hdr[12..16].copy_from_slice(&len_bytes);
        let crc = crc32c::crc32c_append(crc32c::crc32c(&hdr[0..16]), data);
        hdr[16..20].copy_from_slice(&crc.to_le_bytes());
        wrapped.extend_from_slice(&hdr);
        wrapped.extend_from_slice(data);

        let scope = crate::GlobalPool::standalone(2 * 1024 * 1024);
        let mut res2 = scope.register("fuzz_wrapped", true);
        match crate::frame::decode_batch_with_limit(&wrapped, None, Some(&mut res2), 1024 * 1024) {
            Ok(batch) => assert_eq!(res2.size(), batch.memory_bytes()),
            Err(_) => assert_eq!(res2.size(), 0),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use crate::frame::{FrameSlice, decode_batch, decode_next, encode_batch};
    use crate::{
        Batch, BinaryData, Bitmap, COST_DECIMAL_PRECISION, COST_DECIMAL_SCALE, Column, DataType,
        DictColumn, EventsData, Field, FlatColumn, FlatData, Interval, LinksData, MapData,
        RleColumn, ScalarValue, Schema, Selection, Utf8Data,
    };

    use super::*;

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

    fn all_types_schema() -> Arc<Schema> {
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

    fn all_types_batch() -> Batch {
        let schema = all_types_schema();
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
                FlatData::Utf8(Utf8Data::try_from_slices(["hello", "", "é🚀", "world"]).unwrap()),
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
                        Utf8Data::try_from_slices(["k1", "k2", "k3"]).unwrap(),
                        Utf8Data::try_from_slices(["v1", "", "v3"]).unwrap(),
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

    fn encodings_batch() -> Batch {
        let schema = Schema::try_new_arc(vec![
            Field::new("flat_i64", DataType::I64, true),
            Field::new("const_str", DataType::Utf8, false),
            Field::new("const_null", DataType::I64, true),
            Field::new("const_list", DataType::List(Box::new(DataType::I64)), false),
            Field::new("dict_utf8", DataType::Utf8, true),
            Field::new("rle_i64", DataType::I64, true),
        ])
        .unwrap();
        let n = 4usize;

        let flat = Column::Flat(
            FlatColumn::try_new(
                FlatData::I64(vec![10, 0, 30, 40]),
                Some(Bitmap::from_bools(&[true, false, true, true])),
            )
            .unwrap(),
        );
        let const_str =
            Column::try_new_const(ScalarValue::Utf8("constant-string-payload".into()), n).unwrap();
        let const_null = Column::try_new_const(ScalarValue::Null(DataType::I64), n).unwrap();
        let const_list = Column::try_new_const(
            ScalarValue::List {
                item_type: DataType::I64,
                values: vec![
                    ScalarValue::I64(1),
                    ScalarValue::Null(DataType::I64),
                    ScalarValue::I64(3),
                ],
            },
            n,
        )
        .unwrap();
        let dict_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["alpha", "beta"]).unwrap()),
            None,
        )
        .unwrap();
        let dict = Column::Dict(
            DictColumn::try_new(
                dict_vals,
                vec![0, 1, 0, 1],
                Some(Bitmap::from_bools(&[true, false, true, true])),
            )
            .unwrap(),
        );
        let rle_vals = FlatColumn::try_new(
            FlatData::I64(vec![11, 0, 22]),
            Some(Bitmap::from_bools(&[true, false, true])),
        )
        .unwrap();
        let rle = Column::Rle(RleColumn::try_new(rle_vals, vec![2, 3, 4]).unwrap());

        Batch::try_new_all(
            schema,
            vec![flat, const_str, const_null, const_list, dict, rle],
            n,
        )
        .unwrap()
    }

    fn nested_list_batch() -> Batch {
        let nested_ty = DataType::List(Box::new(DataType::List(Box::new(DataType::List(
            Box::new(DataType::Utf8),
        )))));
        let schema = Schema::try_new_arc(vec![
            Field::new("deep_list", nested_ty, true),
            Field::new("events", DataType::Events, true),
            Field::new("links", DataType::Links, true),
        ])
        .unwrap();

        let leaf_utf8 = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["a", "", "bc", "def"]).unwrap()),
            Some(Bitmap::from_bools(&[true, false, true, true])),
        )
        .unwrap();
        let l1 = FlatColumn::try_new(
            FlatData::List {
                item_type: DataType::Utf8,
                offsets: vec![0, 2, 2, 4],
                values: Box::new(Column::Flat(leaf_utf8)),
            },
            Some(Bitmap::from_bools(&[true, false, true])),
        )
        .unwrap();
        let l2 = FlatColumn::try_new(
            FlatData::List {
                item_type: DataType::List(Box::new(DataType::Utf8)),
                offsets: vec![0, 2, 2, 3],
                values: Box::new(Column::Flat(l1)),
            },
            Some(Bitmap::from_bools(&[true, false, true])),
        )
        .unwrap();
        let l3 = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::List(Box::new(DataType::List(Box::new(DataType::Utf8)))),
                    offsets: vec![0, 1, 1, 3],
                    values: Box::new(Column::Flat(l2)),
                },
                Some(Bitmap::from_bools(&[true, false, true])),
            )
            .unwrap(),
        );
        let events = Column::Flat(
            FlatColumn::try_new(
                FlatData::Events(Box::new(
                    EventsData::try_new(
                        vec![0, 1, 1, 1],
                        vec![1_767_225_600_000_000_000],
                        Utf8Data::try_from_slices(["evt"]).unwrap(),
                        Utf8Data::try_from_slices(["{\"k\":\"v\"}"]).unwrap(),
                        Some(Bitmap::from_bools(&[true])),
                    )
                    .unwrap(),
                )),
                Some(Bitmap::from_bools(&[true, false, true])),
            )
            .unwrap(),
        );
        let links = Column::Flat(
            FlatColumn::try_new(
                FlatData::Links(Box::new(
                    LinksData::try_new(
                        vec![0, 0, 0, 1],
                        vec![[0xAB; 16]],
                        vec![[0xCD; 8]],
                        Utf8Data::try_from_slices([""]).unwrap(),
                        Some(Bitmap::from_bools(&[false])),
                    )
                    .unwrap(),
                )),
                Some(Bitmap::from_bools(&[true, false, true])),
            )
            .unwrap(),
        );

        Batch::try_new(
            schema,
            vec![l3, events, links],
            3,
            Selection::Mask(Bitmap::from_bools(&[true, false, true])),
        )
        .unwrap()
    }

    fn small_uncompressed_batch() -> Batch {
        let schema = Schema::try_new_arc(vec![Field::new("x", DataType::I64, false)]).unwrap();
        Batch::try_new_all(
            schema,
            vec![Column::Flat(
                FlatColumn::try_new(FlatData::I64(vec![7]), None).unwrap(),
            )],
            1,
        )
        .unwrap()
    }

    #[test]
    fn replay_column_frame_corpus() {
        replay("column_frame", column_frame);
    }

    #[test]
    fn valid_seeds_decode() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/column_frame");
        let seeds: Vec<PathBuf> = sorted_files(&dir)
            .into_iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("seed-"))
            })
            .collect();
        assert!(!seeds.is_empty(), "no seed-* files in {}", dir.display());

        for path in seeds {
            let data = std::fs::read(&path).expect("read a seed file");
            let mut offset = 0usize;
            let mut count = 0usize;
            loop {
                match decode_next(&data, offset) {
                    FrameSlice::Frame { frame, next } => {
                        decode_batch(&data[frame], None, None)
                            .unwrap_or_else(|e| panic!("seed {} failed: {e}", path.display()));
                        assert!(next > offset);
                        offset = next;
                        count += 1;
                    }
                    FrameSlice::End => break,
                    other => panic!(
                        "seed {} failed decode_next at {offset}: {other:?}",
                        path.display()
                    ),
                }
            }
            assert!(count >= 1, "seed {} had zero frames", path.display());
            if count == 1 {
                decode_batch(&data, None, None)
                    .unwrap_or_else(|e| panic!("single-frame seed {} failed: {e}", path.display()));
            }
        }
    }

    /// Writes the seed corpus for `column_frame` (`just fuzz-seed`).
    #[test]
    #[ignore]
    fn write_seed_corpus() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/column_frame");
        std::fs::create_dir_all(&dir).expect("create the corpus directory");
        let write = |name: &str, bytes: &[u8]| {
            std::fs::write(dir.join(name), bytes).expect("write a seed");
        };

        let mut empty_buf = Vec::new();
        encode_batch(&Batch::empty(all_types_schema()), &mut empty_buf).unwrap();
        write("seed-empty", &empty_buf);

        let mut all_types_buf = Vec::new();
        encode_batch(&all_types_batch(), &mut all_types_buf).unwrap();
        write("seed-all-types", &all_types_buf);

        let mut encodings_buf = Vec::new();
        encode_batch(&encodings_batch(), &mut encodings_buf).unwrap();
        write("seed-encodings", &encodings_buf);

        let mut nested_buf = Vec::new();
        encode_batch(&nested_list_batch(), &mut nested_buf).unwrap();
        write("seed-nested-list", &nested_buf);

        let mut uncompressed_buf = Vec::new();
        encode_batch(&small_uncompressed_batch(), &mut uncompressed_buf).unwrap();
        assert_eq!(uncompressed_buf[5], 0, "seed-uncompressed must set flags=0");
        write("seed-uncompressed", &uncompressed_buf);

        let mut multi_buf = Vec::new();
        multi_buf.extend_from_slice(&uncompressed_buf);
        multi_buf.extend_from_slice(&all_types_buf);
        multi_buf.extend_from_slice(&empty_buf);
        write("seed-multi-frame", &multi_buf);
    }
}
