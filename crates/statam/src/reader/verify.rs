//! Whole-file and sibling `.stpk` chunk pack integrity verification.

use crate::StatamError;
use crate::format::{Encoding, LocalityGroup, MAX_BATCH_DECODED_BYTES, TableKind};
use crate::payload::pack::ChunkPackReader;
use crate::payload::{FLAT_REF_CHUNKS, decode_payload_refs_flat};
use crate::writer::L0_LOCAL_PACK_IDX;

use super::StatamReader;

impl StatamReader {
    /// Performs full whole-file integrity verification of `.statam` and its optional sibling
    /// `.stpk` pack under 1:1 pack ownership:
    ///
    /// - Validates `row_count == expected_rows` and `table_kind == expected_table`.
    /// - Runs [`Self::verify_full`] (header, every data block CRC/decompress, every L0 payload
    ///   frame CRC, and every lazy index CRC).
    /// - When `pack` is `Some((expected_pack_id, pack_reader))`:
    ///   - Requires `meta.schema_meta.pack_ids == [expected_pack_id]`.
    ///   - Calls [`ChunkPackReader::verify_full`] (reads & checks `PackHeader` at offset 0,
    ///     verifies CRCs of all present optional trailer sections, decompresses & verifies CRC
    ///     and chunk bounds of every `.stpk` frame).
    ///   - Validates that every external `PayloadRefRow::Chunks` entry in `.statam` has
    ///     `pack_idx == 0` and `chunk_id < pack_reader.toc().chunks.len()`.
    /// - When `pack` is `None`:
    ///   - Requires `meta.schema_meta.pack_ids.is_empty()` and zero external
    ///     `PayloadRefRow::Chunks` entries (only inline or `L0_LOCAL_PACK_IDX`).
    pub fn verify_full_with_pack(
        &self,
        pack: Option<([u8; 16], &ChunkPackReader)>,
        expected_rows: u64,
        expected_table: TableKind,
    ) -> Result<(), StatamError> {
        if self.meta.postscript.row_count != expected_rows {
            return Err(StatamError::Corrupt {
                reason: "row_count mismatch during verify_full_with_pack",
            });
        }
        if self.meta.schema_meta.table_kind != expected_table {
            return Err(StatamError::Corrupt {
                reason: "table_kind mismatch during verify_full_with_pack",
            });
        }

        self.verify_full()?;

        let expected_pack_chunks = match pack {
            Some((expected_pack_id, pack_reader)) => {
                if self.meta.schema_meta.pack_ids.as_slice() != [expected_pack_id] {
                    return Err(StatamError::Corrupt {
                        reason: "statam pack_ids mismatch during verify_full_with_pack",
                    });
                }
                pack_reader.verify_full(expected_pack_id)?;
                Some(pack_reader.toc().chunks.len())
            }
            None => {
                if !self.meta.schema_meta.pack_ids.is_empty() {
                    return Err(StatamError::Corrupt {
                        reason: "statam has external pack_ids when no pack was provided",
                    });
                }
                None
            }
        };

        // Validate every PayloadRefRow::Chunks reference across all ExternalPayload columns & RGs.
        let num_logical = self.meta.schema_meta.schema.len();
        for cidx in 0..num_logical {
            if self.meta.col_dir[cidx].locality_group != LocalityGroup::ExternalPayload {
                continue;
            }
            for rg_idx in 0..self.meta.rg_dir.len() {
                let rg = &self.meta.rg_dir[rg_idx];
                let blks = self.meta.rg_blocks(cidx, rg_idx);
                for blk in blks {
                    if blk.encoding == Encoding::ConstNull {
                        continue;
                    }
                    let raw = self.read_block_body(blk)?;
                    let refs = decode_payload_refs_flat(
                        &raw,
                        blk.row_count as usize,
                        MAX_BATCH_DECODED_BYTES,
                    )?;
                    for r in 0..refs.len() {
                        if refs.row_kind(r) != FLAT_REF_CHUNKS {
                            continue;
                        }
                        let (pidxs, cids) = refs.chunk_refs(r);
                        for (&pidx, &cid) in pidxs.iter().zip(cids.iter()) {
                            if pidx == L0_LOCAL_PACK_IDX {
                                if rg.l0_payload_len == 0 {
                                    return Err(StatamError::Corrupt {
                                        reason: "L0 payload ref in RG without l0_payload region",
                                    });
                                }
                                continue;
                            }
                            let Some(pack_chunks) = expected_pack_chunks else {
                                return Err(StatamError::Corrupt {
                                    reason: "external payload ref present when no pack was provided",
                                });
                            };
                            if pidx != 0 {
                                return Err(StatamError::Corrupt {
                                    reason: "external payload ref pack_idx != 0 during verify_full_with_pack",
                                });
                            }
                            if (cid as usize) >= pack_chunks {
                                return Err(StatamError::Corrupt {
                                    reason: "external payload ref chunk_id out of pack bounds",
                                });
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;

    use cetra_column::{Batch, Column, DataType, Field, FlatColumn, FlatData, Schema, Utf8Data};

    use crate::codec::WriteProfile;
    use crate::format::{PACK_HEADER_LEN, TableKind};
    use crate::io::SliceReader;
    use crate::payload::pack::{ChunkPackBuilder, ChunkPackReader, PackWriteOptions};
    use crate::reader::StatamReader;
    use crate::writer::{StatamWriter, WriteOptions};

    fn spans_test_schema() -> Arc<Schema> {
        Schema::try_new_arc(vec![
            Field::new("start_time", DataType::TimestampNs { utc: true }, false),
            Field::new("trace_id", DataType::FixedBytes16, false),
            Field::new("input", DataType::Utf8, true),
        ])
        .unwrap()
    }

    fn build_l1_spans_with_pack(
        pack_id: [u8; 16],
        with_dict: bool,
        rows: usize,
    ) -> (Vec<u8>, Vec<u8>, u64) {
        let schema = spans_test_schema();
        let mut ts = Vec::with_capacity(rows);
        let mut tids: Vec<[u8; 16]> = Vec::with_capacity(rows);
        let mut inputs = Vec::with_capacity(rows);
        for i in 0..rows {
            ts.push(1_000_000 + i as i64 * 10);
            tids.push((i as u128 + 1).to_le_bytes());
            // Multi-chunk (> 8 KiB) varied prose so frames compress to >= 1024 B (emitting
            // TrigramSkipSection) and span multiple chunks (emitting EdgeTable).
            let mut s = String::with_capacity(12_000);
            let mut x = (i as u64)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(1);
            for w in 0..800 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let word = match x & 15 {
                    0 => "observability",
                    1 => "latency",
                    2 => "streaming",
                    3 => "checkpoint",
                    4 => "partition",
                    5 => "evaluator",
                    6 => "telemetry",
                    7 => "compaction",
                    8 => "trigram",
                    9 => "payload",
                    10 => "snapshot",
                    11 => "retention",
                    12 => "quarantine",
                    13 => "metadata",
                    14 => "container",
                    _ => "verifier",
                };
                if w % 25 == 0 {
                    s.push_str(&format!("{word}-{}-{w} ", x & 0xFFFF));
                } else {
                    s.push_str(word);
                    s.push(' ');
                }
            }
            inputs.push(s);
        }
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
                Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(tids), None).unwrap()),
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
            rows,
        )
        .unwrap();

        let pack_opts = PackWriteOptions {
            target_frame_raw_bytes: 32 * 1024,
            enable_trigram_skip: true,
            emit_hash_table: true,
            ..PackWriteOptions::default()
        };
        let mut pb = if with_dict {
            ChunkPackBuilder::new_with_dict(pack_id, pack_opts)
        } else {
            ChunkPackBuilder::new(pack_id, pack_opts)
        };
        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        opts.pack_id = Some(pack_id);
        opts.mini_block_size = 32;
        opts.row_group_size = 64;

        let mut statam_bytes = Vec::new();
        {
            let mut w = StatamWriter::new(
                Cursor::new(&mut statam_bytes),
                Arc::clone(&schema),
                opts,
                Some(&mut pb),
            )
            .unwrap();
            w.write_batch(&batch).unwrap();
            w.finish().unwrap();
        }
        let mut pack_bytes = Vec::new();
        pb.finish(&mut pack_bytes).unwrap();
        (statam_bytes, pack_bytes, rows as u64)
    }

    #[test]
    fn verify_full_with_pack_healthy_and_all_corruptions() {
        let pack_id = [0xABu8; 16];
        let (statam_bytes, pack_bytes, rows) = build_l1_spans_with_pack(pack_id, false, 100);

        let reader =
            StatamReader::open(Arc::new(SliceReader::new(statam_bytes.clone())), None).unwrap();
        let pack_reader =
            ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes.clone()))).unwrap();

        // Ensure hash table, trigram section, and edge table are actually present so all checks run.
        let ps = pack_reader.postscript();
        assert!(ps.hash_table_off > 0);
        assert!(ps.trigram_len > 0);
        assert!(ps.edge_table_len > 0);

        // 1. Healthy file + sibling pack passes.
        reader
            .verify_full_with_pack(Some((pack_id, &pack_reader)), rows, TableKind::Spans)
            .unwrap();

        // 2. Wrong expected_rows fails.
        let err = reader
            .verify_full_with_pack(Some((pack_id, &pack_reader)), rows + 1, TableKind::Spans)
            .unwrap_err();
        assert!(err.is_corrupt());

        // 3. Wrong expected_table fails.
        let err = reader
            .verify_full_with_pack(Some((pack_id, &pack_reader)), rows, TableKind::Traces)
            .unwrap_err();
        assert!(err.is_corrupt());

        // 4. pack = None when .statam references pack_ids fails.
        let err = reader
            .verify_full_with_pack(None, rows, TableKind::Spans)
            .unwrap_err();
        assert!(err.is_corrupt());

        // 5. Wrong expected_pack_id fails.
        let err = reader
            .verify_full_with_pack(Some(([0xCDu8; 16], &pack_reader)), rows, TableKind::Spans)
            .unwrap_err();
        assert!(err.is_corrupt());

        // 6. Corrupt PackHeader at offset 0 (pack_id byte flipped).
        {
            let mut bad_pack = pack_bytes.clone();
            bad_pack[16] ^= 0xFF;
            // Open with small tail hint so open() doesn't read header at 0; verify_full_with_pack MUST catch it!
            let bad_pr = ChunkPackReader::open_with_tail_hint(
                Arc::new(SliceReader::new(bad_pack)),
                Some(ps.toc_stored_len),
            )
            .unwrap();
            let err = reader
                .verify_full_with_pack(Some((pack_id, &bad_pr)), rows, TableKind::Spans)
                .unwrap_err();
            assert!(err.is_corrupt());
        }

        // 7. Corrupt ChunkHashTable CRC.
        {
            let mut bad_pack = pack_bytes.clone();
            bad_pack[ps.hash_table_off as usize] ^= 0x55;
            let bad_pr = ChunkPackReader::open(Arc::new(SliceReader::new(bad_pack))).unwrap();
            let err = reader
                .verify_full_with_pack(Some((pack_id, &bad_pr)), rows, TableKind::Spans)
                .unwrap_err();
            assert!(err.is_corrupt());
        }

        // 8. Corrupt TrigramSkipSection CRC.
        {
            let mut bad_pack = pack_bytes.clone();
            bad_pack[ps.trigram_off as usize + 12] ^= 0xAA;
            let bad_pr = ChunkPackReader::open(Arc::new(SliceReader::new(bad_pack))).unwrap();
            let err = reader
                .verify_full_with_pack(Some((pack_id, &bad_pr)), rows, TableKind::Spans)
                .unwrap_err();
            assert!(err.is_corrupt());
        }

        // 9. Corrupt EdgeTable CRC.
        {
            let mut bad_pack = pack_bytes.clone();
            bad_pack[ps.edge_table_off as usize + 16] ^= 0xFF;
            let bad_pr = ChunkPackReader::open(Arc::new(SliceReader::new(bad_pack))).unwrap();
            let err = reader
                .verify_full_with_pack(Some((pack_id, &bad_pr)), rows, TableKind::Spans)
                .unwrap_err();
            assert!(err.is_corrupt());
        }

        // 10. Corrupt .stpk frame payload byte.
        {
            let mut bad_pack = pack_bytes;
            let f0 = &pack_reader.toc().frames[0];
            bad_pack[f0.offset as usize + 16] ^= 0x80;
            let bad_pr = ChunkPackReader::open(Arc::new(SliceReader::new(bad_pack))).unwrap();
            let err = reader
                .verify_full_with_pack(Some((pack_id, &bad_pr)), rows, TableKind::Spans)
                .unwrap_err();
            assert!(err.is_corrupt());
        }

        // 11. Out-of-bounds chunk_id: pack with same pack_id but fewer chunks than .statam references.
        {
            let mut small_pb = ChunkPackBuilder::new(pack_id, PackWriteOptions::default());
            small_pb.intern_chunk(b"only-one-chunk-payload").unwrap();
            let mut small_pack_bytes = Vec::new();
            small_pb.finish(&mut small_pack_bytes).unwrap();
            let small_pr =
                ChunkPackReader::open(Arc::new(SliceReader::new(small_pack_bytes))).unwrap();
            assert!(small_pr.toc().chunks.len() < pack_reader.toc().chunks.len());
            let err = reader
                .verify_full_with_pack(Some((pack_id, &small_pr)), rows, TableKind::Spans)
                .unwrap_err();
            assert!(err.is_corrupt());
        }

        // 12. Corrupt .statam header (opened via cached meta so open doesn't touch offset 0).
        {
            let mut bad_statam = statam_bytes;
            bad_statam[0] ^= 0xFF;
            let bad_r = StatamReader::open_with_shared_meta(
                Arc::new(SliceReader::new(bad_statam)),
                Arc::new(reader.meta().clone()),
                None,
            )
            .unwrap();
            let err = bad_r
                .verify_full_with_pack(Some((pack_id, &pack_reader)), rows, TableKind::Spans)
                .unwrap_err();
            assert!(err.is_corrupt());
        }

        // 13. Healthy L0Fast file (no external pack) passes with pack = None.
        {
            let schema = spans_test_schema();
            let batch = Batch::try_new_all(
                Arc::clone(&schema),
                vec![
                    Column::Flat(
                        FlatColumn::try_new(
                            FlatData::TimestampNs {
                                values: vec![100, 200],
                                utc: true,
                            },
                            None,
                        )
                        .unwrap(),
                    ),
                    Column::Flat(
                        FlatColumn::try_new(FlatData::FixedBytes16(vec![[1u8; 16]; 2]), None)
                            .unwrap(),
                    ),
                    Column::Flat(
                        FlatColumn::try_new(
                            FlatData::Utf8(
                                Utf8Data::try_from_slices(["short", &"L0_LARGE_".repeat(500)])
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
            let opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
            let mut l0_bytes = Vec::new();
            {
                let mut w =
                    StatamWriter::new(Cursor::new(&mut l0_bytes), Arc::clone(&schema), opts, None)
                        .unwrap();
                w.write_batch(&batch).unwrap();
                w.finish().unwrap();
            }
            let l0_reader = StatamReader::open(Arc::new(SliceReader::new(l0_bytes)), None).unwrap();
            l0_reader
                .verify_full_with_pack(None, 2, TableKind::Spans)
                .unwrap();
        }
    }

    #[test]
    fn verify_full_with_pack_detects_corrupt_dict_section() {
        let pack_id = [0xDEu8; 16];
        // Need >= 2 MiB raw (~300 x 8.2 KiB = ~2.4 MiB) to trigger Zstd dictionary training.
        let (statam_bytes, pack_bytes, rows) = build_l1_spans_with_pack(pack_id, true, 300);
        let reader = StatamReader::open(Arc::new(SliceReader::new(statam_bytes)), None).unwrap();
        let pack_reader =
            ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes.clone()))).unwrap();
        reader
            .verify_full_with_pack(Some((pack_id, &pack_reader)), rows, TableKind::Spans)
            .unwrap();

        let ps = pack_reader.postscript();
        #[cfg(not(miri))]
        assert!(
            ps.dict_len > 0,
            "expected Zstd dict training on >= 2 MiB raw payload"
        );
        if ps.dict_len > 0 {
            let mut bad_pack = pack_bytes;
            let corrupt_off = (ps.dict_off as usize + 16).max(PACK_HEADER_LEN);
            bad_pack[corrupt_off] ^= 0xFF;
            let bad_pr = ChunkPackReader::open(Arc::new(SliceReader::new(bad_pack))).unwrap();
            let err = reader
                .verify_full_with_pack(Some((pack_id, &bad_pr)), rows, TableKind::Spans)
                .unwrap_err();
            assert!(err.is_corrupt());
        }
    }
}
