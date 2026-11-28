//! Row Group and mini-block column encoding, shredded maps, blooms, and RG0 initialization.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{Seek, Write};
use std::sync::Arc;

use cetra_column::{Column, DataType, Field, FlatColumn, FlatData, ScalarValue};

use crate::StatamError;
use crate::codec::dict::{DICT_PASSTHROUGH_MAX_BYTES, DictBuilder, DictValueKind, FileDict};
use crate::codec::nested::{encode_shredded_subcol_with_codec, select_shredded_keys, shred_map};
use crate::codec::{
    EncodedMiniBlock, WriteProfile, encode_dict_codes_precomputed, encode_mini_block_with_locality,
};
use crate::format::{
    BLOCK_FLAG_ALL_NULL, BLOCK_FLAG_HAS_NULLS, BLOCK_FLAG_NO_INLINE_PAYLOADS, BlockEntry, Codec,
    Encoding, FramedWriteInfo, LocalityGroup, MAX_BATCH_DECODED_BYTES, MAX_BLOOM_BYTES,
    MAX_COLUMNS, MAX_FRAME_RAW_BYTES, MAX_ROW_GROUPS, MAX_ROW_PAYLOAD_BYTES, MAX_ROWS_PER_FILE,
    RowGroupEntry, ShreddedMapDir, ShreddedMapKeyEntry, TableKind, write_framed_block,
    write_framed_block_adaptive, write_precompressed_framed_block,
};
use crate::parallel::{enter_executor_scope, parallel_map_in_order_static};
use crate::payload::chunker::{
    DedupBypassDetector, INLINE_PAYLOAD_THRESHOLD, chunk_json_array_continuation, chunk_payload,
};
use crate::payload::{
    EncodedPayloadRefs, PayloadRefRow, encode_payload_refs_adaptive, encode_payload_refs_l0_direct,
    encode_payload_refs_l0_from_bytes,
};
use crate::simd::bloom::{BloomIndex, Sbbf, statam_bloom_hash};
use crate::simd::trigram::{MAX_TRIGRAM_BLOOM_BYTES, MIN_TRIGRAM_BLOOM_BYTES, TrigramBloom};

use super::l0_payload::{L0_LOCAL_PACK_IDX, LocalL0PayloadBuilder};
use super::{
    HIGH_CARDINALITY_ID_COLS, HOT_ANALYTICS_POST_TRACE_COLS, HOT_ANALYTICS_TIER1_SMALL_COLS,
    HOT_ANALYTICS_TIER1_TIME_COLS, HOT_ANALYTICS_TIER2_COLS, ID_CORE_COLS,
    KNOWN_LOW_CARD_DICT_COLS, PackBuilderTarget, PhysColWriteState, PrechunkedRefs, Rg0DictResult,
    StatamWriter, default_l1_codec,
};

/// Minimum compressed mini-block bytes required to emit a narrow-column trigram filter.
const MIN_NARROW_TRIGRAM_STORED_BYTES: usize = 2560;
/// Target SBBF false-positive probability.
const BLOOM_FPP: f64 = 0.01;
/// Minimum Row Group rows required before spawning parallel RG worker threads.
pub(super) const MIN_PARALLEL_RG_ROWS: usize = 1_024;
/// Maximum bit density for narrow-column trigram blooms.
const MAX_NARROW_TRIGRAM_DENSITY: f64 = 0.50;

type PayloadPrefixCacheEntry = (usize, usize, Vec<(u16, u32)>);
type PayloadExactCacheEntry = (usize, Vec<(u16, u32)>);

pub(super) enum CoreWriteStep {
    Col(usize),
    MetadataShredded,
    OtherShredded,
}

/// Returns `true` iff every non-null row in `rows` references at least one external `.stpk`
/// chunk and ZERO rows are `Inline` or reference `L0_LOCAL_PACK_IDX`.
#[inline]
pub(super) fn payload_ref_rows_all_external_or_null(rows: &[PayloadRefRow]) -> bool {
    rows.iter().all(|r| match r {
        PayloadRefRow::Null => true,
        PayloadRefRow::Inline(_) => false,
        PayloadRefRow::Chunks(refs) => {
            !refs.is_empty() && refs.iter().all(|&(pidx, _)| pidx != L0_LOCAL_PACK_IDX)
        }
    })
}

/// Returns the deterministic write ordering for `Core` columns and shredded map sub-columns.
///
/// - For `TableKind::Generic`: preserves natural schema order for all `LocalityGroup::Core`
///   columns, followed by `MetadataShredded` and `OtherShredded`.
/// - For Cetra `Spans` / `Traces` / `Scores`: preserves the hardcoded Analytics-Core 6-tier layout.
pub(super) fn core_column_plan(
    table_kind: TableKind,
    fields: &[Field],
    logical_locality: &[LocalityGroup],
) -> Vec<CoreWriteStep> {
    let mut steps = Vec::with_capacity(fields.len() + 2);
    if table_kind == TableKind::Generic {
        for (cidx, _) in fields.iter().enumerate() {
            if logical_locality[cidx] == LocalityGroup::Core {
                steps.push(CoreWriteStep::Col(cidx));
            }
        }
        steps.push(CoreWriteStep::MetadataShredded);
        steps.push(CoreWriteStep::OtherShredded);
        return steps;
    }

    let push_matching = |steps: &mut Vec<CoreWriteStep>, pred: fn(&str) -> bool| {
        for (cidx, field) in fields.iter().enumerate() {
            if logical_locality[cidx] == LocalityGroup::Core && pred(field.name.as_str()) {
                steps.push(CoreWriteStep::Col(cidx));
            }
        }
    };

    // Tier 1A: Small Hot Analytics Core filter/agg columns written FIRST at `core_offset`.
    push_matching(&mut steps, |n| HOT_ANALYTICS_TIER1_SMALL_COLS.contains(&n));
    // Tier 1B: Hot timestamp columns immediately before `metadata` shredded sub-columns.
    push_matching(&mut steps, |n| HOT_ANALYTICS_TIER1_TIME_COLS.contains(&n));
    // Tier 2 (Phase B1): `metadata` shredded sub-columns right inside block #0 (< 1 MiB).
    steps.push(CoreWriteStep::MetadataShredded);
    // Tier 3a: `trace_id` immediately after `metadata.*`.
    push_matching(&mut steps, |n| n == "trace_id");
    // Tier 3b: `cost_total` & `total_tokens` after `trace_id` and before `span_id`.
    push_matching(&mut steps, |n| HOT_ANALYTICS_POST_TRACE_COLS.contains(&n));
    // Tier 3c: Remaining ID Core columns (`span_id`, `score_id`, `parent_span_id`, ...).
    push_matching(&mut steps, |n| n != "trace_id" && ID_CORE_COLS.contains(&n));
    // Tier 4: Tier-2 Hot Analytics Core columns (`duration_ns`, `environment`, `provider`, etc.).
    push_matching(&mut steps, |n| HOT_ANALYTICS_TIER2_COLS.contains(&n));
    // Tier 5 (Phase B2): Remaining shredded sub-columns (`attributes.*`, `resource_attributes.*`).
    steps.push(CoreWriteStep::OtherShredded);
    // Tier 6: Remaining cold Core columns.
    push_matching(&mut steps, |n| {
        !(HOT_ANALYTICS_TIER1_SMALL_COLS.contains(&n)
            || HOT_ANALYTICS_TIER1_TIME_COLS.contains(&n)
            || HOT_ANALYTICS_POST_TRACE_COLS.contains(&n)
            || HOT_ANALYTICS_TIER2_COLS.contains(&n)
            || ID_CORE_COLS.contains(&n))
    });

    steps
}

impl<'a, W: Write + Seek, PW: Write + Seek> StatamWriter<'a, W, PW> {
    pub(super) fn flush_rg_columns(
        &mut self,
        rg_cols: &[Arc<Column>],
        base_row_off: usize,
        rg_rows: usize,
        mut prechunked: Option<PrechunkedRefs<'_>>,
    ) -> Result<(), StatamError> {
        if rg_rows == 0 {
            return Ok(());
        }
        if self.rg_entries.len() >= MAX_ROW_GROUPS {
            return Err(StatamError::BoundsExceeded {
                reason: "row group count exceeds MAX_ROW_GROUPS",
            });
        }
        let next_total = self.total_rows.saturating_add(rg_rows as u64);
        if next_total > MAX_ROWS_PER_FILE {
            return Err(StatamError::BoundsExceeded {
                reason: "total row count exceeds MAX_ROWS_PER_FILE",
            });
        }

        let first_row_u32 =
            u32::try_from(self.total_rows).map_err(|_| StatamError::BoundsExceeded {
                reason: "row_group first_row overflows u32",
            })?;

        let _exec_guard = enter_executor_scope(self.options.executor.as_ref());

        // Gate RG-level worker thread spawning on large RGs so tiny/1-row flushes stay 1T.
        let rg_threads = if rg_rows >= MIN_PARALLEL_RG_ROWS {
            self.options.write_threads
        } else {
            1
        };

        // RG0 initialization: promote ShreddedMap keys (L1Compact) & seed FileDicts
        // BEFORE building RG0 blooms so dict-encoded columns hash only unique dict entries.
        let is_rg0 = !self.rg0_initialized;
        if is_rg0 {
            if rg_threads > 1 {
                self.init_rg0_parallel(rg_cols, base_row_off, rg_rows)?;
            } else {
                self.init_rg0(rg_cols, base_row_off, rg_rows)?;
            }
            self.promote_generic_wide_utf8_cols(rg_cols, base_row_off, rg_rows)?;
            self.rg0_initialized = true;
        }

        // Build per-RG SBBF bloom filters immediately (never accumulate file-wide Vec<u64>).
        let mut bloom_indices: Vec<usize> = self.bloom_col_set.iter().copied().collect();
        bloom_indices.sort_unstable();
        if rg_threads > 1 && bloom_indices.len() > 1 {
            let profile = self.options.profile;
            let bloom_tasks: Vec<_> = bloom_indices
                .into_iter()
                .map(|cidx| {
                    let col_name = self.schema.fields()[cidx].name.clone();
                    let rg0_dict = if is_rg0 {
                        self.phys_col_meta[cidx].dict.clone()
                    } else {
                        None
                    };
                    (cidx, Arc::clone(&rg_cols[cidx]), col_name, rg0_dict)
                })
                .collect();
            let results = parallel_map_in_order_static(
                bloom_tasks,
                rg_threads,
                move |(cidx, col, col_name, rg0_dict)| {
                    let (sbbf, had_any) = build_rg_sbbf(
                        &col,
                        base_row_off,
                        rg_rows,
                        profile,
                        col_name.as_str(),
                        rg0_dict.as_deref(),
                    )?;
                    Ok((cidx, sbbf, had_any))
                },
            )?;
            for (cidx, sbbf, had_any) in results {
                if had_any {
                    self.bloom_has_values.insert(cidx, true);
                }
                if let Some(rgs) = self.bloom_rgs.get_mut(&cidx) {
                    rgs.push(sbbf);
                }
            }
        } else {
            for cidx in bloom_indices {
                let col_name = self.schema.fields()[cidx].name.as_str();
                let rg0_dict = if is_rg0 {
                    self.phys_col_meta[cidx].dict.as_deref()
                } else {
                    None
                };
                let (sbbf, had_any) = build_rg_sbbf(
                    &rg_cols[cidx],
                    base_row_off,
                    rg_rows,
                    self.options.profile,
                    col_name,
                    rg0_dict,
                )?;
                if had_any {
                    self.bloom_has_values.insert(cidx, true);
                }
                if let Some(rgs) = self.bloom_rgs.get_mut(&cidx) {
                    rgs.push(sbbf);
                }
            }
        }

        let zstd_level = self.options.zstd_level;
        let preferred_codec = match self.options.profile {
            WriteProfile::L0Fast => Codec::Lz4,
            WriteProfile::L1Compact => default_l1_codec(),
        };

        let mut core_buf = Vec::new();
        let mut wide_buf = Vec::new();
        let mut l0_local_builder = std::mem::replace(
            &mut self.pending_l0_local,
            LocalL0PayloadBuilder::new_with_threads(zstd_level, self.options.write_threads),
        );

        let core_offset = self.cur_data_offset;
        let mb_size = self.options.mini_block_size;
        let schema = Arc::clone(&self.schema);
        let fields = schema.fields();

        let (core_len, wide_offset) = if rg_threads > 1 {
            self.encode_rg_columns_parallel(
                rg_cols,
                base_row_off,
                rg_rows,
                first_row_u32,
                mb_size,
                core_offset,
                preferred_codec,
                zstd_level,
                prechunked,
                &mut core_buf,
                &mut wide_buf,
                &mut l0_local_builder,
            )?
        } else {
            let mut deferred_wide_residuals: Vec<(usize, Vec<EncodedMiniBlock>, Vec<u16>)> =
                Vec::new();
            for step in core_column_plan(self.options.table_kind, fields, &self.logical_locality) {
                match step {
                    CoreWriteStep::Col(cidx) => {
                        self.encode_rg_scalar_or_nested_col(
                            cidx,
                            &rg_cols[cidx],
                            base_row_off,
                            rg_rows,
                            first_row_u32,
                            mb_size,
                            core_offset,
                            zstd_level,
                            &mut core_buf,
                        )?;
                    }
                    CoreWriteStep::MetadataShredded => {
                        self.encode_shredded_maps_group(
                            rg_cols,
                            base_row_off,
                            rg_rows,
                            first_row_u32,
                            mb_size,
                            core_offset,
                            zstd_level,
                            true,
                            &mut core_buf,
                            &mut deferred_wide_residuals,
                        )?;
                    }
                    CoreWriteStep::OtherShredded => {
                        self.encode_shredded_maps_group(
                            rg_cols,
                            base_row_off,
                            rg_rows,
                            first_row_u32,
                            mb_size,
                            core_offset,
                            zstd_level,
                            false,
                            &mut core_buf,
                            &mut deferred_wide_residuals,
                        )?;
                    }
                }
            }

            let core_len =
                u32::try_from(core_buf.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "row group core_len overflows u32",
                })?;
            let wide_offset = core_offset.checked_add(u64::from(core_len)).ok_or(
                StatamError::BoundsExceeded {
                    reason: "wide_offset overflow",
                },
            )?;

            // Phase C: Write WideRow blocks (regular WideRow cols, deferred shredded residuals, and PayloadRefs).
            for (logical_col, mb_residuals, mb_row_counts) in deferred_wide_residuals {
                let mut cur_first = first_row_u32;
                for (enc, rc) in mb_residuals.iter().zip(mb_row_counts.iter()) {
                    let lz4_cap = if enc.raw_body.len() >= 8 * 1024 {
                        108
                    } else {
                        0
                    };
                    let entry = enc.write_framed_entry_adaptive(
                        cur_first,
                        *rc,
                        wide_offset,
                        zstd_level,
                        lz4_cap,
                        0,
                        &mut wide_buf,
                    )?;
                    self.phys_blocks[logical_col].push(entry);
                    cur_first += u32::from(*rc);
                }
            }

            for (cidx, rg_col) in rg_cols.iter().enumerate() {
                match self.logical_locality[cidx] {
                    LocalityGroup::Core => {}
                    LocalityGroup::WideRow => {
                        if self.logical_to_shredded.contains_key(&cidx) {
                            continue;
                        }
                        self.encode_rg_scalar_or_nested_col(
                            cidx,
                            rg_col,
                            base_row_off,
                            rg_rows,
                            first_row_u32,
                            mb_size,
                            wide_offset,
                            zstd_level,
                            &mut wide_buf,
                        )?;
                    }
                    LocalityGroup::ExternalPayload => {
                        self.encode_rg_payload_col(
                            cidx,
                            rg_col,
                            base_row_off,
                            rg_rows,
                            first_row_u32,
                            mb_size,
                            wide_offset,
                            preferred_codec,
                            zstd_level,
                            prechunked.as_mut().map(|(m, l0)| (*m, l0.as_deref_mut())),
                            &mut wide_buf,
                            &mut l0_local_builder,
                        )?;
                    }
                }
            }
            (core_len, wide_offset)
        };

        if core_buf.len() > MAX_BATCH_DECODED_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "row group core_len exceeds MAX_BATCH_DECODED_BYTES",
            });
        }
        if wide_buf.len() > MAX_BATCH_DECODED_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "row group wide_len exceeds MAX_BATCH_DECODED_BYTES",
            });
        }
        let wide_len = u32::try_from(wide_buf.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "row group wide_len overflows u32",
        })?;
        let after_wide =
            wide_offset
                .checked_add(u64::from(wide_len))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "after_wide offset overflow",
                })?;

        let l0_bytes = l0_local_builder.finish()?;
        if l0_bytes.len() > MAX_BATCH_DECODED_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "row group l0_payload_len exceeds MAX_BATCH_DECODED_BYTES",
            });
        }
        let (l0_payload_offset, l0_payload_len) = if l0_bytes.is_empty() {
            (0u64, 0u32)
        } else {
            let len = u32::try_from(l0_bytes.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "l0_payload_len overflows u32",
            })?;
            (after_wide, len)
        };

        self.rg_entries.push(RowGroupEntry {
            first_row: first_row_u32,
            row_count: rg_rows as u32,
            core_offset,
            core_len,
            wide_len,
            wide_offset,
            l0_payload_offset,
            l0_payload_len,
            rg_flags: 0,
        });

        if !core_buf.is_empty() {
            self.out.write_all(&core_buf)?;
        }
        if !wide_buf.is_empty() {
            self.out.write_all(&wide_buf)?;
        }
        if !l0_bytes.is_empty() {
            self.out.write_all(&l0_bytes)?;
        }

        self.cur_data_offset = after_wide.checked_add(u64::from(l0_payload_len)).ok_or(
            StatamError::BoundsExceeded {
                reason: "cur_data_offset overflow",
            },
        )?;
        self.total_rows = next_total;
        self.core_bytes = self.core_bytes.saturating_add(u64::from(core_len));
        self.wide_bytes = self.wide_bytes.saturating_add(u64::from(wide_len));
        self.l0_payload_bytes = self
            .l0_payload_bytes
            .saturating_add(u64::from(l0_payload_len));
        self.rg0_dict_codes.clear();

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_rg_columns_parallel(
        &mut self,
        rg_cols: &[Arc<Column>],
        base_row_off: usize,
        rg_rows: usize,
        first_row_u32: u32,
        mb_size: usize,
        core_offset: u64,
        preferred_codec: Codec,
        zstd_level: i32,
        mut prechunked: Option<PrechunkedRefs<'_>>,
        core_buf: &mut Vec<u8>,
        wide_buf: &mut Vec<u8>,
        l0_local_builder: &mut LocalL0PayloadBuilder,
    ) -> Result<(u32, u64), StatamError> {
        let num_logical = self.schema.len();
        let num_shredded = self.shredded_maps.len();
        let mut rg0_codes_map = std::mem::take(&mut self.rg0_dict_codes);

        enum RgEncodeTask {
            ShreddedMap {
                s_idx: usize,
                sm: ShreddedMapDir,
                is_metadata: bool,
                col: Arc<Column>,
            },
            ScalarOrNested {
                cidx: usize,
                col: Arc<Column>,
                dt: DataType,
                is_wide_row: bool,
                dict: Option<Arc<FileDict>>,
                track_trigram: bool,
                precomputed_rg0: Option<(Vec<u32>, Option<cetra_column::Bitmap>)>,
            },
        }

        enum RgEncodeResult {
            ShreddedMap(usize, EncodedShreddedMap),
            ScalarOrNested(usize, EncodedScalarCol),
        }

        // Schedule heaviest tasks (shredded maps + WideRow cols) before small Core cols.
        let mut tasks = Vec::with_capacity(num_shredded + num_logical);
        for s_idx in 0..num_shredded {
            let sm = self.shredded_maps[s_idx].clone();
            let logical_col = sm.logical_col_idx as usize;
            let is_metadata = self.schema.fields()[logical_col].name == "metadata";
            tasks.push(RgEncodeTask::ShreddedMap {
                s_idx,
                sm,
                is_metadata,
                col: Arc::clone(&rg_cols[logical_col]),
            });
        }
        for (cidx, col) in rg_cols.iter().enumerate().take(num_logical) {
            if self.logical_locality[cidx] == LocalityGroup::WideRow
                && !self.logical_to_shredded.contains_key(&cidx)
            {
                tasks.push(RgEncodeTask::ScalarOrNested {
                    cidx,
                    col: Arc::clone(col),
                    dt: self.schema.fields()[cidx].data_type.clone(),
                    is_wide_row: true,
                    dict: self.phys_col_meta[cidx].dict.clone(),
                    track_trigram: self.narrow_trigram_col_set.contains(&cidx),
                    precomputed_rg0: rg0_codes_map.remove(&cidx),
                });
            }
        }
        for (cidx, col) in rg_cols.iter().enumerate().take(num_logical) {
            if self.logical_locality[cidx] == LocalityGroup::Core {
                tasks.push(RgEncodeTask::ScalarOrNested {
                    cidx,
                    col: Arc::clone(col),
                    dt: self.schema.fields()[cidx].data_type.clone(),
                    is_wide_row: false,
                    dict: self.phys_col_meta[cidx].dict.clone(),
                    track_trigram: self.narrow_trigram_col_set.contains(&cidx),
                    precomputed_rg0: rg0_codes_map.remove(&cidx),
                });
            }
        }

        let profile = self.options.profile;

        let encoded = parallel_map_in_order_static(
            tasks,
            self.options.write_threads,
            move |task| match task {
                RgEncodeTask::ShreddedMap {
                    s_idx,
                    sm,
                    is_metadata,
                    col,
                } => {
                    let res = encode_shredded_map_to_local(
                        &sm,
                        is_metadata,
                        &col,
                        base_row_off,
                        rg_rows,
                        first_row_u32,
                        mb_size,
                        profile,
                        zstd_level,
                    )?;
                    Ok(RgEncodeResult::ShreddedMap(s_idx, res))
                }
                RgEncodeTask::ScalarOrNested {
                    cidx,
                    col,
                    dt,
                    is_wide_row,
                    dict,
                    track_trigram,
                    precomputed_rg0,
                } => {
                    let mut local_buf = Vec::new();
                    let mut blocks = Vec::new();
                    let mut local_tris = Vec::new();
                    let (used_dict_encoding, has_non_dict_data_block, null_count_total) =
                        encode_scalar_or_nested_col_into(
                            &col,
                            &dt,
                            profile,
                            is_wide_row,
                            dict.as_deref(),
                            precomputed_rg0,
                            base_row_off,
                            rg_rows,
                            first_row_u32,
                            mb_size,
                            0,
                            zstd_level,
                            &mut local_buf,
                            &mut blocks,
                            track_trigram.then_some(&mut local_tris),
                        )?;
                    Ok(RgEncodeResult::ScalarOrNested(
                        cidx,
                        EncodedScalarCol {
                            local_buf,
                            blocks,
                            used_dict_encoding,
                            has_non_dict_data_block,
                            null_count_total,
                            narrow_trigrams: track_trigram.then_some(local_tris),
                        },
                    ))
                }
            },
        )?;

        let mut col_results: Vec<Option<EncodedScalarCol>> =
            (0..num_logical).map(|_| None).collect();
        let mut shred_results: Vec<Option<EncodedShreddedMap>> =
            (0..num_shredded).map(|_| None).collect();
        for item in encoded {
            match item {
                RgEncodeResult::ScalarOrNested(cidx, res) => {
                    col_results[cidx] = Some(res);
                }
                RgEncodeResult::ShreddedMap(s_idx, res) => {
                    shred_results[s_idx] = Some(res);
                }
            }
        }

        let commit_core_col = |cidx: usize,
                               col_results: &mut [Option<EncodedScalarCol>],
                               phys_col_meta: &mut [PhysColWriteState],
                               phys_blocks: &mut [Vec<BlockEntry>],
                               narrow_trigrams: &mut HashMap<usize, Vec<Option<Vec<u8>>>>,
                               core_buf: &mut Vec<u8>|
         -> Result<(), StatamError> {
            if let Some(res) = col_results[cidx].take() {
                commit_scalar_col_result(
                    phys_col_meta,
                    phys_blocks,
                    narrow_trigrams,
                    cidx,
                    res,
                    core_offset,
                    core_buf,
                )?;
            }
            Ok(())
        };

        let schema = Arc::clone(&self.schema);
        let fields = schema.fields();

        for step in core_column_plan(self.options.table_kind, fields, &self.logical_locality) {
            match step {
                CoreWriteStep::Col(cidx) => {
                    commit_core_col(
                        cidx,
                        &mut col_results,
                        &mut self.phys_col_meta,
                        &mut self.phys_blocks,
                        &mut self.narrow_trigrams,
                        core_buf,
                    )?;
                }
                CoreWriteStep::MetadataShredded => {
                    for (sm, slot) in self.shredded_maps.iter_mut().zip(shred_results.iter_mut()) {
                        if let Some(shred) = slot.as_mut()
                            && shred.is_metadata
                        {
                            commit_shredded_subcols(
                                sm,
                                &mut self.phys_blocks,
                                shred,
                                core_offset,
                                core_buf,
                            )?;
                        }
                    }
                }
                CoreWriteStep::OtherShredded => {
                    for (sm, slot) in self.shredded_maps.iter_mut().zip(shred_results.iter_mut()) {
                        if let Some(shred) = slot.as_mut()
                            && !shred.is_metadata
                        {
                            commit_shredded_subcols(
                                sm,
                                &mut self.phys_blocks,
                                shred,
                                core_offset,
                                core_buf,
                            )?;
                        }
                    }
                }
            }
        }

        let core_len = u32::try_from(core_buf.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "row group core_len overflows u32",
        })?;
        let wide_offset =
            core_offset
                .checked_add(u64::from(core_len))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "wide_offset overflow",
                })?;

        // Phase C: Deferred shredded map residuals (B1 then B2), then WideRow & ExternalPayload in cidx order.
        for slot in &mut shred_results {
            if slot.as_ref().is_some_and(|s| s.is_metadata)
                && let Some(shred) = slot.take()
            {
                self.phys_col_meta[shred.logical_col].null_count_total = self.phys_col_meta
                    [shred.logical_col]
                    .null_count_total
                    .saturating_add(shred.res_null_count_total);
                append_col_blocks(
                    wide_buf,
                    wide_offset,
                    &shred.local_wide_buf,
                    shred.residual_blocks,
                    &mut self.phys_blocks[shred.logical_col],
                )?;
            }
        }
        for slot in &mut shred_results {
            if let Some(shred) = slot.take() {
                self.phys_col_meta[shred.logical_col].null_count_total = self.phys_col_meta
                    [shred.logical_col]
                    .null_count_total
                    .saturating_add(shred.res_null_count_total);
                append_col_blocks(
                    wide_buf,
                    wide_offset,
                    &shred.local_wide_buf,
                    shred.residual_blocks,
                    &mut self.phys_blocks[shred.logical_col],
                )?;
            }
        }

        for (cidx, rg_col) in rg_cols.iter().enumerate() {
            match self.logical_locality[cidx] {
                LocalityGroup::Core => {}
                LocalityGroup::WideRow => {
                    if let Some(res) = col_results[cidx].take() {
                        commit_scalar_col_result(
                            &mut self.phys_col_meta,
                            &mut self.phys_blocks,
                            &mut self.narrow_trigrams,
                            cidx,
                            res,
                            wide_offset,
                            wide_buf,
                        )?;
                    }
                }
                LocalityGroup::ExternalPayload => {
                    self.encode_rg_payload_col(
                        cidx,
                        rg_col,
                        base_row_off,
                        rg_rows,
                        first_row_u32,
                        mb_size,
                        wide_offset,
                        preferred_codec,
                        zstd_level,
                        prechunked.as_mut().map(|(m, l0)| (*m, l0.as_deref_mut())),
                        wide_buf,
                        l0_local_builder,
                    )?;
                }
            }
        }

        Ok((core_len, wide_offset))
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_shredded_maps_group(
        &mut self,
        rg_cols: &[Arc<Column>],
        base_row_off: usize,
        rg_rows: usize,
        first_row_u32: u32,
        mb_size: usize,
        core_offset: u64,
        zstd_level: i32,
        want_metadata: bool,
        core_buf: &mut Vec<u8>,
        deferred_wide_residuals: &mut Vec<(usize, Vec<EncodedMiniBlock>, Vec<u16>)>,
    ) -> Result<(), StatamError> {
        let num_shredded = self.shredded_maps.len();
        for s_idx in 0..num_shredded {
            let logical_col = self.shredded_maps[s_idx].logical_col_idx as usize;
            let is_metadata = self.schema.fields()[logical_col].name == "metadata";
            if is_metadata != want_metadata {
                continue;
            }
            let promoted_keys: Vec<String> = self.shredded_maps[s_idx]
                .keys
                .iter()
                .map(|k| k.key.clone())
                .collect();
            let sub_col_indices: Vec<usize> = self.shredded_maps[s_idx]
                .keys
                .iter()
                .map(|k| k.sub_col_idx as usize)
                .collect();

            let mut mb_residuals = Vec::new();
            let mut mb_row_counts = Vec::new();

            let mut row_off = 0usize;
            while row_off < rg_rows {
                let mb_len = (rg_rows - row_off).min(mb_size);
                let mb_first_row = first_row_u32 + row_off as u32;
                let abs_off = base_row_off + row_off;
                let sliced_owned;
                let sliced: &Column = if abs_off == 0 && mb_len == rg_cols[logical_col].len() {
                    &rg_cols[logical_col]
                } else {
                    sliced_owned = rg_cols[logical_col].slice(abs_off, mb_len, None)?;
                    &sliced_owned
                };
                let flat = as_flat_cow(sliced)?;
                let shredded = shred_map(&flat, &promoted_keys)?;

                for (k_idx, &sub_pidx) in sub_col_indices.iter().enumerate() {
                    self.shredded_maps[s_idx].keys[k_idx].residual_count =
                        self.shredded_maps[s_idx].keys[k_idx]
                            .residual_count
                            .saturating_add(shredded.residual_counts[k_idx]);

                    let (sub_codec, sub_raw) = encode_shredded_subcol_with_codec(
                        &shredded.sub_cols[k_idx],
                        self.options.profile,
                    )?;
                    let blk_off = core_offset + core_buf.len() as u64;
                    let info = write_framed_block_adaptive(
                        Encoding::ShreddedMap,
                        &sub_raw,
                        sub_codec,
                        zstd_level,
                        114,
                        0,
                        core_buf,
                    )?;
                    let absent_count = u16::try_from(
                        mb_len.saturating_sub(shredded.sub_cols[k_idx].in_map.count_ones()),
                    )
                    .unwrap_or(u16::MAX);
                    self.phys_blocks[sub_pidx].push(BlockEntry {
                        first_row: mb_first_row,
                        row_count: mb_len as u16,
                        null_count: absent_count,
                        offset: blk_off,
                        stored_len: info.stored_len,
                        raw_len: info.raw_len,
                        encoding: Encoding::ShreddedMap,
                        codec: info.codec,
                        block_flags: 0,
                        min_inline_len: 0,
                        max_inline_len: 0,
                        reserved: [0; 3],
                        min_inline: [0; 16],
                        max_inline: [0; 16],
                    });
                }

                let res_enc = crate::codec::encode_shredded_residual_block(
                    &shredded.residual,
                    self.options.profile,
                )?;
                self.phys_col_meta[logical_col].null_count_total = self.phys_col_meta[logical_col]
                    .null_count_total
                    .saturating_add(u32::from(res_enc.null_count));
                mb_residuals.push(res_enc);
                mb_row_counts.push(mb_len as u16);

                row_off += mb_len;
            }

            deferred_wide_residuals.push((logical_col, mb_residuals, mb_row_counts));
        }
        Ok(())
    }

    fn init_rg0(
        &mut self,
        rg_cols: &[Arc<Column>],
        base_row_off: usize,
        rg_rows: usize,
    ) -> Result<(), StatamError> {
        let schema = Arc::clone(&self.schema);
        let fields = schema.fields();

        // 1. Shredded maps in L1Compact.
        if self.options.profile == WriteProfile::L1Compact {
            for (cidx, field) in fields.iter().enumerate() {
                if field.data_type != DataType::MapUtf8 {
                    continue;
                }
                let promoted = extract_rg0_shredded_keys(&rg_cols[cidx], base_row_off, rg_rows)?;
                if promoted.is_empty() {
                    continue;
                }
                self.register_shredded_map(cidx, promoted)?;
            }
        }

        // 2. Seed file-level dictionaries from RG0 on low-cardinality columns.
        for (cidx, field) in fields.iter().enumerate() {
            if !is_rg0_dict_candidate(field, self.logical_locality[cidx], self.options.profile) {
                continue;
            }
            if let Some((dict, codes, nulls)) = eval_rg0_dict_candidate(
                &rg_cols[cidx],
                field.name.as_str(),
                self.logical_locality[cidx],
                self.options.profile,
                base_row_off,
                rg_rows,
            )? {
                let dict_len = dict.len();
                self.phys_col_meta[cidx].dict = Some(Arc::new(dict));
                if dict_len >= 2 {
                    self.rg0_dict_codes.insert(cidx, (codes, nulls));
                }
            }
        }

        Ok(())
    }

    /// Freezes RG0 `WideRow` promotions once for `TableKind::Generic`: any non-dict `Core` `Utf8`
    /// column whose RG0 mean raw length exceeds `128 B` and whose RG0 raw bytes are `>= 256 KiB`
    /// is promoted to `LocalityGroup::WideRow` file-wide (identical across RG0 and all `rg_idx >= 1`).
    fn promote_generic_wide_utf8_cols(
        &mut self,
        rg_cols: &[Arc<Column>],
        base_row_off: usize,
        rg_rows: usize,
    ) -> Result<(), StatamError> {
        if self.options.table_kind != TableKind::Generic || rg_rows == 0 {
            return Ok(());
        }
        let schema = Arc::clone(&self.schema);
        for (cidx, field) in schema.fields().iter().enumerate() {
            if self.logical_locality[cidx] != LocalityGroup::Core
                || field.data_type != DataType::Utf8
                || self.phys_col_meta[cidx].dict.is_some()
            {
                continue;
            }
            let (raw_bytes, non_null) = match rg_cols[cidx].as_ref() {
                Column::Flat(f) => {
                    let FlatData::Utf8(u) = f.data() else {
                        continue;
                    };
                    let offs = u.offsets();
                    let raw =
                        offs[base_row_off + rg_rows].saturating_sub(offs[base_row_off]) as usize;
                    let nn = match f.nulls() {
                        Some(bm) => bm.slice(base_row_off, rg_rows)?.count_ones(),
                        None => rg_rows,
                    };
                    (raw, nn)
                }
                other => {
                    let sliced = other.slice(base_row_off, rg_rows, None)?;
                    let flat = sliced.materialize(None)?;
                    let FlatData::Utf8(u) = flat.data() else {
                        continue;
                    };
                    let raw = u.bytes().len();
                    let nn = match flat.nulls() {
                        Some(bm) => bm.count_ones(),
                        None => rg_rows,
                    };
                    (raw, nn)
                }
            };
            if raw_bytes >= 256 * 1024 && non_null > 0 && raw_bytes > non_null.saturating_mul(128) {
                self.logical_locality[cidx] = LocalityGroup::WideRow;
                self.phys_col_meta[cidx].locality_group = LocalityGroup::WideRow;
            }
        }
        Ok(())
    }

    fn init_rg0_parallel(
        &mut self,
        rg_cols: &[Arc<Column>],
        base_row_off: usize,
        rg_rows: usize,
    ) -> Result<(), StatamError> {
        enum Rg0InitTask {
            ShreddedMap(usize, Arc<Column>),
            DictCandidate(usize, Arc<Column>, String, LocalityGroup),
        }

        enum Rg0InitResult {
            ShreddedMap(usize, Vec<String>),
            DictCandidate(usize, Option<Rg0DictResult>),
        }

        let schema = Arc::clone(&self.schema);
        let fields = schema.fields();
        let mut tasks = Vec::new();
        if self.options.profile == WriteProfile::L1Compact {
            for (cidx, field) in fields.iter().enumerate() {
                if field.data_type == DataType::MapUtf8 {
                    tasks.push(Rg0InitTask::ShreddedMap(cidx, Arc::clone(&rg_cols[cidx])));
                }
            }
        }
        for (cidx, field) in fields.iter().enumerate() {
            if is_rg0_dict_candidate(field, self.logical_locality[cidx], self.options.profile) {
                tasks.push(Rg0InitTask::DictCandidate(
                    cidx,
                    Arc::clone(&rg_cols[cidx]),
                    field.name.clone(),
                    self.logical_locality[cidx],
                ));
            }
        }

        let profile = self.options.profile;
        let results = parallel_map_in_order_static(
            tasks,
            self.options.write_threads,
            move |task| match task {
                Rg0InitTask::ShreddedMap(cidx, col) => {
                    let promoted = extract_rg0_shredded_keys(&col, base_row_off, rg_rows)?;
                    Ok(Rg0InitResult::ShreddedMap(cidx, promoted))
                }
                Rg0InitTask::DictCandidate(cidx, col, col_name, locality) => {
                    let res = eval_rg0_dict_candidate(
                        &col,
                        col_name.as_str(),
                        locality,
                        profile,
                        base_row_off,
                        rg_rows,
                    )?;
                    Ok(Rg0InitResult::DictCandidate(cidx, res))
                }
            },
        )?;

        for res in results {
            match res {
                Rg0InitResult::ShreddedMap(cidx, promoted) => {
                    if !promoted.is_empty() {
                        self.register_shredded_map(cidx, promoted)?;
                    }
                }
                Rg0InitResult::DictCandidate(cidx, Some((dict, codes, nulls))) => {
                    let dict_len = dict.len();
                    self.phys_col_meta[cidx].dict = Some(Arc::new(dict));
                    if dict_len >= 2 {
                        self.rg0_dict_codes.insert(cidx, (codes, nulls));
                    }
                }
                Rg0InitResult::DictCandidate(_, None) => {}
            }
        }

        Ok(())
    }

    fn register_shredded_map(
        &mut self,
        cidx: usize,
        promoted: Vec<String>,
    ) -> Result<(), StatamError> {
        let mut key_entries = Vec::with_capacity(promoted.len());
        for key in promoted {
            let sub_idx = self.phys_col_meta.len();
            if sub_idx >= MAX_COLUMNS {
                return Err(StatamError::BoundsExceeded {
                    reason: "physical column count with shredded maps exceeds MAX_COLUMNS",
                });
            }
            let sub_u16 = sub_idx as u16;
            self.phys_col_meta.push(PhysColWriteState {
                phys_col_id: sub_u16,
                logical_col_id: cidx as u16,
                locality_group: LocalityGroup::Core,
                is_shredded_subcol: true,
                is_payload_refs: false,
                dict: None,
                used_dict_encoding: false,
                has_non_dict_data_block: false,
                null_count_total: 0,
            });
            self.phys_blocks.push(Vec::new());
            key_entries.push(ShreddedMapKeyEntry {
                key,
                sub_col_idx: sub_u16,
                residual_count: 0,
            });
        }

        let sm_idx = self.shredded_maps.len();
        self.shredded_maps.push(ShreddedMapDir {
            logical_col_idx: cidx as u16,
            keys: key_entries,
        });
        self.logical_to_shredded.insert(cidx, sm_idx);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_rg_scalar_or_nested_col(
        &mut self,
        cidx: usize,
        col: &Column,
        base_row_off: usize,
        rg_rows: usize,
        first_row_u32: u32,
        mb_size: usize,
        base_offset: u64,
        zstd_level: i32,
        out_buf: &mut Vec<u8>,
    ) -> Result<(), StatamError> {
        let dt = self.schema.fields()[cidx].data_type.clone();
        let precomputed_rg0 = self.rg0_dict_codes.remove(&cidx);
        let dict_clone = self.phys_col_meta[cidx].dict.as_ref().map(Arc::clone);
        let is_wide_row = self.logical_locality[cidx] == LocalityGroup::WideRow;
        let out_trigrams = self.narrow_trigrams.get_mut(&cidx);
        let (used_dict, has_non_dict, null_total) = encode_scalar_or_nested_col_into(
            col,
            &dt,
            self.options.profile,
            is_wide_row,
            dict_clone.as_deref(),
            precomputed_rg0,
            base_row_off,
            rg_rows,
            first_row_u32,
            mb_size,
            base_offset,
            zstd_level,
            out_buf,
            &mut self.phys_blocks[cidx],
            out_trigrams,
        )?;
        if used_dict {
            self.phys_col_meta[cidx].used_dict_encoding = true;
        }
        if has_non_dict {
            self.phys_col_meta[cidx].has_non_dict_data_block = true;
        }
        self.phys_col_meta[cidx].null_count_total = self.phys_col_meta[cidx]
            .null_count_total
            .saturating_add(null_total);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_rg_payload_col(
        &mut self,
        cidx: usize,
        col: &Column,
        base_row_off: usize,
        rg_rows: usize,
        first_row_u32: u32,
        mb_size: usize,
        base_offset: u64,
        preferred_codec: Codec,
        zstd_level: i32,
        mut prechunked: Option<PrechunkedRefs<'_>>,
        wide_buf: &mut Vec<u8>,
        l0_local: &mut LocalL0PayloadBuilder,
    ) -> Result<(), StatamError> {
        if prechunked.is_none()
            && (self.pending_l0_mbs.contains_key(&cidx)
                || self.pending_l0_partial.contains_key(&cidx))
        {
            let mut mbs = self.pending_l0_mbs.remove(&cidx).unwrap_or_default();
            if let Some(partial) = self.pending_l0_partial.remove(&cidx)
                && !partial.is_empty()
            {
                let mb_len = partial.len();
                let null_count_usize = partial
                    .iter()
                    .filter(|r| matches!(r, PayloadRefRow::Null))
                    .count();
                let null_count = u16::try_from(null_count_usize).unwrap_or(u16::MAX);
                let (extra_flags, raw_body) = if null_count_usize == mb_len {
                    (0u8, EncodedPayloadRefs::empty())
                } else {
                    let rb = if self.options.profile == WriteProfile::L0Fast {
                        let mut v = Vec::new();
                        encode_payload_refs_l0_direct(&partial, &mut v)?;
                        EncodedPayloadRefs::Raw(v)
                    } else {
                        encode_payload_refs_adaptive(&partial, self.options.profile, zstd_level)?
                    };
                    let f = if payload_ref_rows_all_external_or_null(&partial) {
                        BLOCK_FLAG_NO_INLINE_PAYLOADS
                    } else {
                        0
                    };
                    (f, rb)
                };
                mbs.push((mb_len as u16, null_count, extra_flags, raw_body));
            }

            let mut cur_first = first_row_u32;
            for (mb_len_u16, null_count, extra_flags, encoded) in mbs {
                self.phys_col_meta[cidx].null_count_total = self.phys_col_meta[cidx]
                    .null_count_total
                    .saturating_add(u32::from(null_count));
                if null_count == mb_len_u16 {
                    self.phys_blocks[cidx].push(BlockEntry {
                        first_row: cur_first,
                        row_count: mb_len_u16,
                        null_count,
                        offset: 0,
                        stored_len: 0,
                        raw_len: 0,
                        encoding: Encoding::ConstNull,
                        codec: Codec::None,
                        block_flags: BLOCK_FLAG_ALL_NULL | BLOCK_FLAG_HAS_NULLS,
                        min_inline_len: 0,
                        max_inline_len: 0,
                        reserved: [0; 3],
                        min_inline: [0; 16],
                        max_inline: [0; 16],
                    });
                } else {
                    let blk_off = base_offset + wide_buf.len() as u64;
                    let info = write_encoded_payload_refs_block(
                        &encoded,
                        self.options.profile,
                        preferred_codec,
                        zstd_level,
                        wide_buf,
                    )?;
                    let block_flags = if null_count > 0 {
                        BLOCK_FLAG_HAS_NULLS
                    } else {
                        0
                    } | extra_flags;
                    self.phys_blocks[cidx].push(BlockEntry {
                        first_row: cur_first,
                        row_count: mb_len_u16,
                        null_count,
                        offset: blk_off,
                        stored_len: info.stored_len,
                        raw_len: info.raw_len,
                        encoding: Encoding::PayloadRefs,
                        codec: info.codec,
                        block_flags,
                        min_inline_len: 0,
                        max_inline_len: 0,
                        reserved: [0; 3],
                        min_inline: [0; 16],
                        max_inline: [0; 16],
                    });
                }
                cur_first += u32::from(mb_len_u16);
            }
            return Ok(());
        }

        if prechunked.is_none()
            && (self.options.profile == WriteProfile::L0Fast || self.pack_builder.is_none())
        {
            if let Column::Const { value, .. } = col
                && value.is_null()
            {
                let mut row_off = 0usize;
                while row_off < rg_rows {
                    let mb_len = (rg_rows - row_off).min(mb_size);
                    let mb_u16 = mb_len as u16;
                    self.phys_col_meta[cidx].null_count_total = self.phys_col_meta[cidx]
                        .null_count_total
                        .saturating_add(u32::from(mb_u16));
                    self.phys_blocks[cidx].push(BlockEntry {
                        first_row: first_row_u32 + row_off as u32,
                        row_count: mb_u16,
                        null_count: mb_u16,
                        offset: 0,
                        stored_len: 0,
                        raw_len: 0,
                        encoding: Encoding::ConstNull,
                        codec: Codec::None,
                        block_flags: BLOCK_FLAG_ALL_NULL | BLOCK_FLAG_HAS_NULLS,
                        min_inline_len: 0,
                        max_inline_len: 0,
                        reserved: [0; 3],
                        min_inline: [0; 16],
                        max_inline: [0; 16],
                    });
                    row_off += mb_len;
                }
                return Ok(());
            }

            let fc = as_flat_cow(col)?;
            let (offsets, all_bytes, is_binary): (&[u32], &[u8], bool) = match fc.data() {
                FlatData::Utf8(u) => (u.offsets(), u.bytes(), false),
                FlatData::Binary(b) => (b.offsets(), b.bytes(), true),
                _ => {
                    return Err(StatamError::SchemaMismatch {
                        reason: "ExternalPayload column must be DataType::Utf8 or DataType::Binary",
                    });
                }
            };
            let col_nulls = fc.nulls();
            let mut raw_body = Vec::new();
            let mut row_off = 0usize;
            while row_off < rg_rows {
                let mb_len = (rg_rows - row_off).min(mb_size);
                let mb_first_row = first_row_u32 + row_off as u32;
                raw_body.clear();
                let null_count = encode_payload_refs_l0_from_bytes(
                    offsets,
                    all_bytes,
                    col_nulls,
                    base_row_off + row_off,
                    mb_len,
                    INLINE_PAYLOAD_THRESHOLD,
                    L0_LOCAL_PACK_IDX,
                    |bytes, cids| {
                        if is_binary {
                            l0_local.add_blob_into(bytes, cids)
                        } else {
                            for slice in bytes.chunks(MAX_FRAME_RAW_BYTES) {
                                cids.push(l0_local.add_chunk(slice)?);
                            }
                            Ok(())
                        }
                    },
                    &mut raw_body,
                )?;
                self.phys_col_meta[cidx].null_count_total = self.phys_col_meta[cidx]
                    .null_count_total
                    .saturating_add(u32::from(null_count));
                if usize::from(null_count) == mb_len {
                    self.phys_blocks[cidx].push(BlockEntry {
                        first_row: mb_first_row,
                        row_count: mb_len as u16,
                        null_count,
                        offset: 0,
                        stored_len: 0,
                        raw_len: 0,
                        encoding: Encoding::ConstNull,
                        codec: Codec::None,
                        block_flags: BLOCK_FLAG_ALL_NULL | BLOCK_FLAG_HAS_NULLS,
                        min_inline_len: 0,
                        max_inline_len: 0,
                        reserved: [0; 3],
                        min_inline: [0; 16],
                        max_inline: [0; 16],
                    });
                } else {
                    let block_codec = payload_refs_block_codec(
                        self.options.profile,
                        preferred_codec,
                        raw_body.len(),
                    );
                    let blk_off = base_offset + wide_buf.len() as u64;
                    let info = write_framed_block(
                        Encoding::PayloadRefs,
                        &raw_body,
                        block_codec,
                        zstd_level,
                        wide_buf,
                    )?;
                    let block_flags = if null_count > 0 {
                        BLOCK_FLAG_HAS_NULLS
                    } else {
                        0
                    };
                    self.phys_blocks[cidx].push(BlockEntry {
                        first_row: mb_first_row,
                        row_count: mb_len as u16,
                        null_count,
                        offset: blk_off,
                        stored_len: info.stored_len,
                        raw_len: info.raw_len,
                        encoding: Encoding::PayloadRefs,
                        codec: info.codec,
                        block_flags,
                        min_inline_len: 0,
                        max_inline_len: 0,
                        reserved: [0; 3],
                        min_inline: [0; 16],
                        max_inline: [0; 16],
                    });
                }
                row_off += mb_len;
            }
            return Ok(());
        }

        let flat = if prechunked.is_some() {
            None
        } else {
            Some(as_flat_cow(col)?)
        };

        let mut row_off = 0usize;
        while row_off < rg_rows {
            let mb_len = (rg_rows - row_off).min(mb_size);
            let mb_first_row = first_row_u32 + row_off as u32;

            let ref_rows = if let Some((ref_map, ref mut local_source)) = prechunked {
                if let Some(rows) = ref_map.get(&cidx) {
                    let slice = &rows[row_off..row_off + mb_len];
                    let mut remapped = Vec::with_capacity(slice.len());
                    for row in slice {
                        match row {
                            PayloadRefRow::Null => remapped.push(PayloadRefRow::Null),
                            PayloadRefRow::Inline(b) => {
                                remapped.push(PayloadRefRow::Inline(b.clone()));
                            }
                            PayloadRefRow::Chunks(refs) => {
                                let mut out_refs = Vec::with_capacity(refs.len());
                                for &(pidx, cid) in refs {
                                    if pidx == L0_LOCAL_PACK_IDX {
                                        let l0_src = local_source.as_deref_mut().ok_or(
                                            StatamError::Corrupt {
                                                reason: "missing L0 local chunk during rewrite",
                                            },
                                        )?;
                                        let bytes = l0_src.read_chunk(cid)?;
                                        if self.schema.fields()[cidx].data_type == DataType::Binary
                                        {
                                            let new_cids = l0_local.add_blob(bytes)?;
                                            for new_cid in new_cids {
                                                out_refs.push((L0_LOCAL_PACK_IDX, new_cid));
                                            }
                                        } else {
                                            let new_cid = l0_local.add_chunk(bytes)?;
                                            out_refs.push((L0_LOCAL_PACK_IDX, new_cid));
                                        }
                                    } else {
                                        self.uses_external_pack = true;
                                        out_refs.push((pidx, cid));
                                    }
                                }
                                remapped.push(PayloadRefRow::Chunks(out_refs));
                            }
                        }
                    }
                    remapped
                } else {
                    vec![PayloadRefRow::Null; mb_len]
                }
            } else {
                let fc = flat.as_ref().ok_or(StatamError::Corrupt {
                    reason: "missing materialized payload column",
                })?;
                self.build_payload_ref_rows(cidx, fc, base_row_off + row_off, mb_len, l0_local)?
            };

            let null_count_usize = ref_rows
                .iter()
                .filter(|r| matches!(r, PayloadRefRow::Null))
                .count();
            let null_count = u16::try_from(null_count_usize).unwrap_or(u16::MAX);
            self.phys_col_meta[cidx].null_count_total = self.phys_col_meta[cidx]
                .null_count_total
                .saturating_add(u32::from(null_count));

            if null_count_usize == mb_len {
                self.phys_blocks[cidx].push(BlockEntry {
                    first_row: mb_first_row,
                    row_count: mb_len as u16,
                    null_count,
                    offset: 0,
                    stored_len: 0,
                    raw_len: 0,
                    encoding: Encoding::ConstNull,
                    codec: Codec::None,
                    block_flags: BLOCK_FLAG_ALL_NULL | BLOCK_FLAG_HAS_NULLS,
                    min_inline_len: 0,
                    max_inline_len: 0,
                    reserved: [0; 3],
                    min_inline: [0; 16],
                    max_inline: [0; 16],
                });
            } else {
                let encoded = if self.options.profile == WriteProfile::L0Fast {
                    let mut raw_body = Vec::new();
                    encode_payload_refs_l0_direct(&ref_rows, &mut raw_body)?;
                    EncodedPayloadRefs::Raw(raw_body)
                } else {
                    encode_payload_refs_adaptive(&ref_rows, self.options.profile, zstd_level)?
                };
                let blk_off = base_offset + wide_buf.len() as u64;
                let info = write_encoded_payload_refs_block(
                    &encoded,
                    self.options.profile,
                    preferred_codec,
                    zstd_level,
                    wide_buf,
                )?;
                let block_flags = if null_count > 0 {
                    BLOCK_FLAG_HAS_NULLS
                } else {
                    0
                } | if payload_ref_rows_all_external_or_null(&ref_rows) {
                    BLOCK_FLAG_NO_INLINE_PAYLOADS
                } else {
                    0
                };
                self.phys_blocks[cidx].push(BlockEntry {
                    first_row: mb_first_row,
                    row_count: mb_len as u16,
                    null_count,
                    offset: blk_off,
                    stored_len: info.stored_len,
                    raw_len: info.raw_len,
                    encoding: Encoding::PayloadRefs,
                    codec: info.codec,
                    block_flags,
                    min_inline_len: 0,
                    max_inline_len: 0,
                    reserved: [0; 3],
                    min_inline: [0; 16],
                    max_inline: [0; 16],
                });
            }

            row_off += mb_len;
        }
        Ok(())
    }

    pub(super) fn build_payload_ref_rows(
        &mut self,
        cidx: usize,
        flat: &FlatColumn,
        row_off: usize,
        mb_len: usize,
        l0_local: &mut LocalL0PayloadBuilder,
    ) -> Result<Vec<PayloadRefRow>, StatamError> {
        let nulls = flat.nulls();
        if let FlatData::Binary(b) = flat.data() {
            let mut out = Vec::with_capacity(mb_len);
            for r in row_off..row_off + mb_len {
                if nulls.is_some_and(|bm| !bm.get(r)) {
                    out.push(PayloadRefRow::Null);
                    continue;
                }
                let bytes = b.get(r).unwrap_or(&[]);
                if bytes.len() > MAX_ROW_PAYLOAD_BYTES {
                    return Err(StatamError::BoundsExceeded {
                        reason: "row payload exceeds MAX_ROW_PAYLOAD_BYTES",
                    });
                }
                if bytes.len() <= INLINE_PAYLOAD_THRESHOLD {
                    out.push(PayloadRefRow::Inline(bytes.to_vec()));
                    continue;
                }
                if self.options.profile == WriteProfile::L1Compact
                    && let Some(pb) = self.pack_builder.as_mut().map(PackBuilderTarget::as_mut)
                {
                    self.uses_external_pack = true;
                    let cids = pb.append_blob(bytes)?;
                    // Skip `mark_boundary_chunks`: Binary blobs never emit or use `EdgeTable`.
                    let refs = cids.into_iter().map(|cid| (0u16, cid)).collect();
                    out.push(PayloadRefRow::Chunks(refs));
                } else {
                    let cids = l0_local.add_blob(bytes)?;
                    let refs = cids
                        .into_iter()
                        .map(|cid| (L0_LOCAL_PACK_IDX, cid))
                        .collect();
                    out.push(PayloadRefRow::Chunks(refs));
                }
            }
            return Ok(out);
        }

        let FlatData::Utf8(u) = flat.data() else {
            return Err(StatamError::SchemaMismatch {
                reason: "ExternalPayload column must be DataType::Utf8 or DataType::Binary",
            });
        };
        let mut out = Vec::with_capacity(mb_len);
        let mut chunk_scratch = Vec::new();
        let mut boundary_cids_scratch: Vec<u32> = Vec::new();
        let mut prefix_mru: Vec<PayloadPrefixCacheEntry> = Vec::with_capacity(2);
        let mut prefix_table: Vec<Option<PayloadPrefixCacheEntry>> = Vec::new();
        let mut exact_cache: [Option<PayloadExactCacheEntry>; 64] = std::array::from_fn(|_| None);

        for r in row_off..row_off + mb_len {
            if nulls.is_some_and(|bm| !bm.get(r)) {
                out.push(PayloadRefRow::Null);
                continue;
            }
            let bytes = crate::codec::utf8_bytes_at(u, r).unwrap_or(&[]);
            if bytes.len() > MAX_ROW_PAYLOAD_BYTES {
                return Err(StatamError::BoundsExceeded {
                    reason: "row payload exceeds MAX_ROW_PAYLOAD_BYTES",
                });
            }
            if bytes.len() <= INLINE_PAYLOAD_THRESHOLD {
                out.push(PayloadRefRow::Inline(bytes.to_vec()));
                continue;
            }

            if self.options.profile == WriteProfile::L1Compact
                && let Some(pb) = self.pack_builder.as_mut().map(PackBuilderTarget::as_mut)
            {
                self.uses_external_pack = true;
                let bypassed = self
                    .bypass_detectors
                    .get(&cidx)
                    .is_some_and(DedupBypassDetector::should_bypass);
                let mut refs = Vec::new();

                if bypassed {
                    // W4 bypass: skip CDC hashing, dedup map, and trigrams; omit edge bytes for single-chunk payloads.
                    let bypass_chunk_max = MAX_FRAME_RAW_BYTES.min(512 * 1024);
                    let single_chunk = bytes.len() <= bypass_chunk_max;
                    for slice in bytes.chunks(bypass_chunk_max) {
                        let cid = pb.append_raw_bypass_with_edge(slice, !single_chunk)?;
                        refs.push((0u16, cid));
                    }
                } else {
                    let need_sample = !self
                        .bypass_detectors
                        .get(&cidx)
                        .is_some_and(DedupBypassDetector::is_decided);
                    let exact_slot = if !need_sample && bytes.len() >= 16 {
                        let mut h = (bytes.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
                        let h0 = u64::from_le_bytes([
                            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                            bytes[7],
                        ]);
                        let h1 = u64::from_le_bytes([
                            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13],
                            bytes[14], bytes[15],
                        ]);
                        let t0 = bytes.len() - 16;
                        let h2 = u64::from_le_bytes([
                            bytes[t0],
                            bytes[t0 + 1],
                            bytes[t0 + 2],
                            bytes[t0 + 3],
                            bytes[t0 + 4],
                            bytes[t0 + 5],
                            bytes[t0 + 6],
                            bytes[t0 + 7],
                        ]);
                        let t1 = bytes.len() - 8;
                        let h3 = u64::from_le_bytes([
                            bytes[t1],
                            bytes[t1 + 1],
                            bytes[t1 + 2],
                            bytes[t1 + 3],
                            bytes[t1 + 4],
                            bytes[t1 + 5],
                            bytes[t1 + 6],
                            bytes[t1 + 7],
                        ]);
                        h = (h ^ h0).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                        h = (h ^ h1).wrapping_mul(0x94d0_49bb_1331_11eb);
                        h = (h ^ h2).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                        h = (h ^ h3).wrapping_mul(0x94d0_49bb_1331_11eb);
                        h ^= h >> 31;
                        Some((h as usize) & 63)
                    } else {
                        None
                    };

                    if let Some(slot) = exact_slot
                        && let Some((prev_row, prev_refs)) = &exact_cache[slot]
                        && crate::codec::utf8_bytes_at(u, *prev_row).unwrap_or(&[]) == bytes
                    {
                        out.push(PayloadRefRow::Chunks(prev_refs.clone()));
                        continue;
                    }

                    let mut cache_hit = false;
                    let mut mru_hit = false;
                    let prefix_slot = if !need_sample && bytes.len() >= 128 {
                        let mut h = 0x517c_c1b7_2722_0a95u64;
                        for &chunk in bytes[..128].as_chunks::<8>().0 {
                            let v = u64::from_le_bytes(chunk);
                            h = (h ^ v).wrapping_mul(0x9e37_79b9_7f4a_7c15);
                        }
                        h ^= h >> 31;
                        Some((h as usize) & 4095)
                    } else {
                        None
                    };

                    if !need_sample {
                        let table_hit = if !prefix_table.is_empty() {
                            prefix_slot.and_then(|slot| prefix_table[slot].as_ref())
                        } else {
                            None
                        };
                        for (idx_in_iter, (prev_row_idx, prefix_byte_len, prefix_refs)) in
                            prefix_mru.iter().rev().chain(table_hit).enumerate()
                        {
                            let prefix_len = *prefix_byte_len;
                            if bytes.len() > prefix_len + 1 && bytes[prefix_len] == b',' {
                                let prev_bytes =
                                    crate::codec::utf8_bytes_at(u, *prev_row_idx).unwrap_or(&[]);
                                if prev_bytes.len() >= prefix_len {
                                    let edge = prefix_len.min(8);
                                    if bytes[..edge] == prev_bytes[..edge]
                                        && bytes[prefix_len - edge..prefix_len]
                                            == prev_bytes[prefix_len - edge..prefix_len]
                                        && bytes[..prefix_len] == prev_bytes[..prefix_len]
                                    {
                                        refs.extend_from_slice(prefix_refs);
                                        chunk_json_array_continuation(
                                            &bytes[prefix_len..],
                                            &mut chunk_scratch,
                                        );
                                        for &chunk in &chunk_scratch {
                                            let cid = pb.intern_chunk(chunk)?;
                                            refs.push((0u16, cid));
                                        }
                                        cache_hit = true;
                                        mru_hit = idx_in_iter < prefix_mru.len();
                                        break;
                                    }
                                }
                            }
                        }
                        if !cache_hit
                            && let Some((prefix_len, prefix_refs)) = pb.lookup_prefix(bytes)
                        {
                            refs.extend_from_slice(prefix_refs);
                            chunk_json_array_continuation(&bytes[prefix_len..], &mut chunk_scratch);
                            for &chunk in &chunk_scratch {
                                let cid = pb.intern_chunk(chunk)?;
                                refs.push((0u16, cid));
                            }
                            cache_hit = true;
                        }
                    }

                    if !cache_hit {
                        chunk_payload(bytes, &mut chunk_scratch);
                        let detector = self.bypass_detectors.entry(cidx).or_default();
                        let need_sample = !detector.is_decided();
                        for &chunk in &chunk_scratch {
                            let cid = pb.intern_chunk(chunk)?;
                            if need_sample {
                                let is_hit = match self.max_chunk_id_seen.get(&cidx).copied() {
                                    Some(max_cid) => cid <= max_cid,
                                    None => false,
                                };
                                let max_entry = self.max_chunk_id_seen.entry(cidx).or_insert(cid);
                                if cid > *max_entry {
                                    *max_entry = cid;
                                }
                                let stored_len = if chunk.len() > 32 {
                                    lz4_flex::block::compress(chunk).len().min(chunk.len())
                                } else {
                                    chunk.len()
                                };
                                detector.record(is_hit, chunk.len(), stored_len);
                            }
                            refs.push((0u16, cid));
                        }
                    }

                    if let Some(&last_chunk) = chunk_scratch.last()
                        && (last_chunk == b"]".as_slice() || last_chunk.starts_with(b","))
                        && refs.len() >= 2
                        && bytes.ends_with(b"]")
                        && bytes.len() > last_chunk.len()
                    {
                        if prefix_mru.len() == 2 {
                            prefix_mru.remove(0);
                        }
                        let prefix_byte_len = bytes.len() - last_chunk.len();
                        let prefix_refs = &refs[..refs.len() - 1];
                        if !mru_hit && !need_sample {
                            pb.insert_prefix(&bytes[..prefix_byte_len], prefix_refs);
                        }
                        let entry = (r, prefix_byte_len, prefix_refs.to_vec());
                        if let Some(slot) = prefix_slot {
                            if prefix_table.is_empty() {
                                prefix_table = vec![None; 4096];
                            }
                            prefix_table[slot] = Some(entry.clone());
                        }
                        prefix_mru.push(entry);
                    }
                    if let Some(slot) = exact_slot {
                        exact_cache[slot] = Some((r, refs.clone()));
                    }
                }
                if refs.len() > 1 {
                    boundary_cids_scratch.clear();
                    boundary_cids_scratch.extend(refs.iter().map(|&(_, cid)| cid));
                    pb.mark_boundary_chunks(&boundary_cids_scratch);
                }
                out.push(PayloadRefRow::Chunks(refs));
            } else {
                // L0Fast (or L1Compact with pack_builder == None): self-contained local frames.
                let mut refs = Vec::new();
                for slice in bytes.chunks(MAX_FRAME_RAW_BYTES) {
                    let cid = l0_local.add_chunk(slice)?;
                    refs.push((L0_LOCAL_PACK_IDX, cid));
                }
                out.push(PayloadRefRow::Chunks(refs));
            }
        }

        Ok(out)
    }

    pub(super) fn take_and_build_bloom_for_col(
        &mut self,
        cidx: usize,
    ) -> Result<Option<Vec<u8>>, StatamError> {
        let Some(rgs) = self.bloom_rgs.remove(&cidx) else {
            return Ok(None);
        };
        let has_any = self.bloom_has_values.remove(&cidx).unwrap_or(false);
        if !has_any || rgs.is_empty() {
            return Ok(None);
        }
        let bytes = BloomIndex::PerRowGroup(rgs).to_bytes();
        if bytes.len() > MAX_BLOOM_BYTES {
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    pub(super) fn take_and_build_primary_bloom(
        &mut self,
        cidx: usize,
        avail_bytes: usize,
    ) -> Result<Option<(Vec<u8>, bool)>, StatamError> {
        let Some(rgs) = self.bloom_rgs.remove(&cidx) else {
            return Ok(None);
        };
        let has_any = self.bloom_has_values.remove(&cidx).unwrap_or(false);
        if !has_any || rgs.is_empty() {
            return Ok(None);
        }
        let bytes = BloomIndex::PerRowGroup(rgs).to_bytes();
        if bytes.len() > MAX_BLOOM_BYTES {
            return Ok(None);
        }
        let fits_hot = bytes.len() <= avail_bytes;
        Ok(Some((bytes, fits_hot)))
    }
}

pub(super) fn as_flat_cow(col: &Column) -> Result<Cow<'_, FlatColumn>, StatamError> {
    match col {
        Column::Flat(fc) => Ok(Cow::Borrowed(fc)),
        other => Ok(Cow::Owned(other.materialize(None)?)),
    }
}

#[inline]
fn bloom_fpp_for_ndv(ndv: usize) -> f64 {
    if ndv > 80_000 { 0.04 } else { BLOOM_FPP }
}

fn build_rg_sbbf(
    col: &Column,
    base_row_off: usize,
    rg_rows: usize,
    profile: WriteProfile,
    col_name: &str,
    rg0_dict: Option<&FileDict>,
) -> Result<(Sbbf, bool), StatamError> {
    if rg_rows == 0 || col.is_empty() {
        return Ok((Sbbf::new_for_ndv(1, BLOOM_FPP), false));
    }

    // RG0 dict shortcut: hash only the unique dictionary entries directly.
    if let Some(dict) = rg0_dict {
        let dict_len = dict.len();
        if dict_len == 0 {
            return Ok((Sbbf::new_for_ndv(1, BLOOM_FPP), false));
        }
        let mut sbbf = Sbbf::new_for_ndv(dict_len, bloom_fpp_for_ndv(dict_len));
        let mut had_any = false;
        let dict_vals = dict.values();
        for idx in 0..dict_len {
            if let Some(h) = flat_row_bloom_hash(dict_vals, idx) {
                sbbf.insert_hash(h);
                had_any = true;
            }
        }
        return Ok((sbbf, had_any));
    }

    // Fast path for L0Fast Flat columns and unique ID columns (`span_id`, `score_id`):
    // estimate ndv = non-null count without sorting Vec<u64>.
    if (profile == WriteProfile::L0Fast || matches!(col_name, "span_id" | "score_id"))
        && let Column::Flat(f) = col
    {
        let end = base_row_off + rg_rows;
        let non_nulls = match f.nulls() {
            None => rg_rows,
            Some(bm) if base_row_off == 0 && rg_rows == f.len() => {
                rg_rows.saturating_sub(bm.count_zeros())
            }
            Some(bm) => (base_row_off..end).filter(|&r| bm.get(r)).count(),
        };
        if non_nulls == 0 {
            return Ok((Sbbf::new_for_ndv(1, BLOOM_FPP), false));
        }
        let mut sbbf = Sbbf::new_for_ndv(non_nulls, bloom_fpp_for_ndv(non_nulls));
        let had_any = insert_flat_bloom_range(f, base_row_off, end, &mut sbbf);
        return Ok((sbbf, had_any));
    }

    let mut hashes = Vec::new();
    if base_row_off == 0 && rg_rows == col.len() {
        collect_column_bloom_hashes(col, &mut hashes)?;
    } else {
        let sliced = col.slice(base_row_off, rg_rows, None)?;
        collect_column_bloom_hashes(&sliced, &mut hashes)?;
    }
    if hashes.is_empty() {
        return Ok((Sbbf::new_for_ndv(1, BLOOM_FPP), false));
    }
    if profile == WriteProfile::L1Compact {
        hashes.sort_unstable();
        hashes.dedup();
    }
    let mut sbbf = Sbbf::new_for_ndv(hashes.len(), bloom_fpp_for_ndv(hashes.len()));
    for h in hashes {
        sbbf.insert_hash(h);
    }
    Ok((sbbf, true))
}

struct EncodedScalarCol {
    local_buf: Vec<u8>,
    blocks: Vec<BlockEntry>,
    used_dict_encoding: bool,
    has_non_dict_data_block: bool,
    null_count_total: u32,
    narrow_trigrams: Option<Vec<Option<Vec<u8>>>>,
}

struct EncodedShreddedMap {
    logical_col: usize,
    is_metadata: bool,
    local_core_buf: Vec<u8>,
    sub_col_indices: Vec<usize>,
    sub_blocks: Vec<Vec<BlockEntry>>,
    residual_counts: Vec<u32>,
    res_null_count_total: u32,
    local_wide_buf: Vec<u8>,
    residual_blocks: Vec<BlockEntry>,
}

fn append_col_blocks(
    dst_buf: &mut Vec<u8>,
    group_base_offset: u64,
    local_buf: &[u8],
    blocks: Vec<BlockEntry>,
    dst_blocks: &mut Vec<BlockEntry>,
) -> Result<(), StatamError> {
    let col_rel_off = dst_buf.len() as u64;
    let abs_base =
        group_base_offset
            .checked_add(col_rel_off)
            .ok_or(StatamError::BoundsExceeded {
                reason: "column block offset overflow",
            })?;
    dst_buf.extend_from_slice(local_buf);
    for mut blk in blocks {
        if !matches!(blk.encoding, Encoding::ConstNull | Encoding::Const) {
            blk.offset = blk
                .offset
                .checked_add(abs_base)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "block offset overflow",
                })?;
        }
        dst_blocks.push(blk);
    }
    Ok(())
}

fn commit_scalar_col_result(
    phys_col_meta: &mut [PhysColWriteState],
    phys_blocks: &mut [Vec<BlockEntry>],
    narrow_trigrams: &mut HashMap<usize, Vec<Option<Vec<u8>>>>,
    cidx: usize,
    res: EncodedScalarCol,
    group_base_offset: u64,
    dst_buf: &mut Vec<u8>,
) -> Result<(), StatamError> {
    if res.used_dict_encoding {
        phys_col_meta[cidx].used_dict_encoding = true;
    }
    if res.has_non_dict_data_block {
        phys_col_meta[cidx].has_non_dict_data_block = true;
    }
    phys_col_meta[cidx].null_count_total = phys_col_meta[cidx]
        .null_count_total
        .saturating_add(res.null_count_total);
    if let Some(tris) = res.narrow_trigrams
        && let Some(list) = narrow_trigrams.get_mut(&cidx)
    {
        list.extend(tris);
    }
    append_col_blocks(
        dst_buf,
        group_base_offset,
        &res.local_buf,
        res.blocks,
        &mut phys_blocks[cidx],
    )
}

fn commit_shredded_subcols(
    shredded_map: &mut ShreddedMapDir,
    phys_blocks: &mut [Vec<BlockEntry>],
    shred: &mut EncodedShreddedMap,
    core_offset: u64,
    core_buf: &mut Vec<u8>,
) -> Result<(), StatamError> {
    let col_rel_off = core_buf.len() as u64;
    let abs_base = core_offset
        .checked_add(col_rel_off)
        .ok_or(StatamError::BoundsExceeded {
            reason: "shredded subcol block offset overflow",
        })?;
    core_buf.extend_from_slice(&shred.local_core_buf);
    drop(std::mem::take(&mut shred.local_core_buf));
    for (k_idx, &sub_pidx) in shred.sub_col_indices.iter().enumerate() {
        shredded_map.keys[k_idx].residual_count = shredded_map.keys[k_idx]
            .residual_count
            .saturating_add(shred.residual_counts[k_idx]);
        for mut blk in std::mem::take(&mut shred.sub_blocks[k_idx]) {
            if !matches!(blk.encoding, Encoding::ConstNull | Encoding::Const) {
                blk.offset =
                    blk.offset
                        .checked_add(abs_base)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "block offset overflow",
                        })?;
            }
            phys_blocks[sub_pidx].push(blk);
        }
    }
    Ok(())
}

fn extract_rg0_shredded_keys(
    col: &Column,
    base_row_off: usize,
    rg_rows: usize,
) -> Result<Vec<String>, StatamError> {
    let sliced_owned;
    let col_ref = if base_row_off == 0 && rg_rows == col.len() {
        col
    } else {
        sliced_owned = col.slice(base_row_off, rg_rows, None)?;
        &sliced_owned
    };
    let flat = as_flat_cow(col_ref)?;
    let FlatData::MapUtf8(map) = flat.data() else {
        return Ok(Vec::new());
    };
    Ok(select_shredded_keys(map))
}

fn is_rg0_dict_candidate(field: &Field, locality: LocalityGroup, profile: WriteProfile) -> bool {
    if locality == LocalityGroup::ExternalPayload {
        return false;
    }
    if DictValueKind::from_data_type(&field.data_type).is_none() {
        return false;
    }
    if HIGH_CARDINALITY_ID_COLS.contains(&field.name.as_str()) {
        return false;
    }
    if profile == WriteProfile::L0Fast
        && (!KNOWN_LOW_CARD_DICT_COLS.contains(&field.name.as_str())
            || matches!(field.name.as_str(), "session_id" | "user_id"))
    {
        return false;
    }
    true
}

fn eval_rg0_dict_candidate(
    col: &Column,
    field_name: &str,
    locality: LocalityGroup,
    profile: WriteProfile,
    base_row_off: usize,
    rg_rows: usize,
) -> Result<Option<Rg0DictResult>, StatamError> {
    let sliced_owned;
    let col_ref = if base_row_off == 0 && rg_rows == col.len() {
        col
    } else {
        sliced_owned = col.slice(base_row_off, rg_rows, None)?;
        &sliced_owned
    };
    let flat = as_flat_cow(col_ref)?;
    let is_known_low_card = KNOWN_LOW_CARD_DICT_COLS.contains(&field_name);
    let is_core = locality == LocalityGroup::Core;
    let is_wide_non_known = locality == LocalityGroup::WideRow && !is_known_low_card;
    let max_entries = match profile {
        WriteProfile::L0Fast => 512,
        WriteProfile::L1Compact => {
            if is_known_low_card || is_core {
                16_384
            } else if is_wide_non_known {
                1_024
            } else {
                4_096
            }
        }
    };
    let builder = match profile {
        WriteProfile::L0Fast => DictBuilder::l0_with_max(max_entries),
        WriteProfile::L1Compact => DictBuilder::l1_with_max(max_entries),
    };
    let Some((dict, codes)) = builder.build(&flat)? else {
        return Ok(None);
    };
    if is_wide_non_known && (dict.len() > 1_024 || dict.memory_bytes() > DICT_PASSTHROUGH_MAX_BYTES)
    {
        return Ok(None);
    }
    let non_null_rows = flat
        .len()
        .saturating_sub(flat.nulls().map_or(0, |bm| bm.count_zeros()));
    let ratio_den = if dict.len() > 4_096 { 4 } else { 2 };
    let low_ndv = if is_known_low_card {
        dict.len() <= max_entries
            && (non_null_rows < 16 || dict.len() * ratio_den <= non_null_rows + 4)
    } else {
        dict.len() <= max_entries
            && ((non_null_rows >= 16 && dict.len() * ratio_den <= non_null_rows) || dict.len() <= 8)
    };
    if low_ndv {
        Ok(Some((dict, codes, flat.nulls().cloned())))
    } else {
        Ok(None)
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_scalar_or_nested_col_into(
    col: &Column,
    dt: &DataType,
    profile: WriteProfile,
    is_wide_row: bool,
    dict: Option<&FileDict>,
    precomputed_rg0: Option<(Vec<u32>, Option<cetra_column::Bitmap>)>,
    base_row_off: usize,
    rg_rows: usize,
    first_row_u32: u32,
    mb_size: usize,
    base_offset: u64,
    zstd_level: i32,
    out_buf: &mut Vec<u8>,
    out_blocks: &mut Vec<BlockEntry>,
    mut out_trigrams: Option<&mut Vec<Option<Vec<u8>>>>,
) -> Result<(bool, bool, u32), StatamError> {
    let mut used_dict_encoding = false;
    let mut has_non_dict_data_block = false;
    let mut null_count_total = 0u32;

    let precomputed_rg_n = if precomputed_rg0.is_none()
        && rg_rows >= 256
        && let Some(d) = dict
    {
        let rg_slice_owned;
        let rg_slice = if base_row_off == 0 && rg_rows == col.len() {
            col
        } else {
            rg_slice_owned = col.slice(base_row_off, rg_rows, None)?;
            &rg_slice_owned
        };
        if let Ok(flat) = as_flat_cow(rg_slice)
            && let Some(codes) = d.try_map_column(&flat)?
        {
            Some((codes, flat.nulls().cloned()))
        } else {
            None
        }
    } else {
        None
    };

    if let Some((rg0_codes, rg0_nulls)) = precomputed_rg0.or(precomputed_rg_n) {
        let mut row_off = 0usize;
        while row_off < rg_rows {
            let mb_len = (rg_rows - row_off).min(mb_size);
            let mb_first_row = first_row_u32 + row_off as u32;
            let mb_nulls_owned;
            let mb_nulls = match &rg0_nulls {
                None => None,
                Some(bm) => {
                    let mut sub_bm = cetra_column::Bitmap::new_ones(mb_len);
                    let mut has_null = false;
                    for i in 0..mb_len {
                        if !bm.get(row_off + i) {
                            let _ = sub_bm.set(i, false);
                            has_null = true;
                        }
                    }
                    if has_null {
                        mb_nulls_owned = sub_bm;
                        Some(&mb_nulls_owned)
                    } else {
                        None
                    }
                }
            };
            let enc = encode_dict_codes_precomputed(
                &rg0_codes[row_off..row_off + mb_len],
                mb_nulls,
                profile,
                0,
            )?;
            if enc.encoding == Encoding::Dict {
                used_dict_encoding = true;
            }
            null_count_total = null_count_total.saturating_add(u32::from(enc.null_count));
            let (lz4_pct_cap, min_savings_pct) = if profile == WriteProfile::L1Compact {
                (112, 12)
            } else {
                (0, 0)
            };
            let entry = enc.write_framed_entry_adaptive(
                mb_first_row,
                mb_len as u16,
                base_offset,
                zstd_level,
                lz4_pct_cap,
                min_savings_pct,
                out_buf,
            )?;
            out_blocks.push(entry);
            if let Some(list) = out_trigrams.as_deref_mut() {
                list.push(None);
            }
            row_off += mb_len;
        }
        return Ok((
            used_dict_encoding,
            has_non_dict_data_block,
            null_count_total,
        ));
    }

    // O(1) mini-block fast path for `Column::Const` (all-null or inline <=16B / empty collection).
    if let Column::Const { value, .. } = col {
        if value.is_null() || matches!(dt, DataType::Null) {
            let mut row_off = 0usize;
            while row_off < rg_rows {
                let mb_len = (rg_rows - row_off).min(mb_size);
                let mb_u16 = mb_len as u16;
                null_count_total = null_count_total.saturating_add(mb_len as u32);
                out_blocks.push(BlockEntry {
                    first_row: first_row_u32 + row_off as u32,
                    row_count: mb_u16,
                    null_count: mb_u16,
                    offset: 0,
                    stored_len: 0,
                    raw_len: 0,
                    encoding: Encoding::ConstNull,
                    codec: Codec::None,
                    block_flags: BLOCK_FLAG_ALL_NULL | BLOCK_FLAG_HAS_NULLS,
                    min_inline_len: 0,
                    max_inline_len: 0,
                    reserved: [0; 3],
                    min_inline: [0u8; 16],
                    max_inline: [0u8; 16],
                });
                if let Some(list) = out_trigrams.as_deref_mut() {
                    list.push(None);
                }
                row_off += mb_len;
            }
            return Ok((
                used_dict_encoding,
                has_non_dict_data_block,
                null_count_total,
            ));
        }

        let single = Column::Const {
            value: value.clone(),
            len: 1,
        };
        let template = encode_mini_block_with_locality(&single, dt, profile, dict, is_wide_row)?;
        if template.encoding == Encoding::Const {
            let mut row_off = 0usize;
            while row_off < rg_rows {
                let mb_len = (rg_rows - row_off).min(mb_size);
                out_blocks.push(BlockEntry {
                    first_row: first_row_u32 + row_off as u32,
                    row_count: mb_len as u16,
                    null_count: 0,
                    offset: 0,
                    stored_len: 0,
                    raw_len: 0,
                    encoding: Encoding::Const,
                    codec: Codec::None,
                    block_flags: template.block_flags,
                    min_inline_len: template.min_inline_len,
                    max_inline_len: template.max_inline_len,
                    reserved: [0; 3],
                    min_inline: template.min_inline,
                    max_inline: template.max_inline,
                });
                if let Some(list) = out_trigrams.as_deref_mut() {
                    list.push(None);
                }
                row_off += mb_len;
            }
            return Ok((
                used_dict_encoding,
                has_non_dict_data_block,
                null_count_total,
            ));
        }
    }

    let mut row_off = 0usize;
    while row_off < rg_rows {
        let mb_len = (rg_rows - row_off).min(mb_size);
        let mb_first_row = first_row_u32 + row_off as u32;
        let abs_off = base_row_off + row_off;
        let sliced_owned;
        let sliced = if abs_off == 0 && mb_len == col.len() {
            col
        } else {
            sliced_owned = col.slice(abs_off, mb_len, None)?;
            &sliced_owned
        };
        let enc = encode_mini_block_with_locality(sliced, dt, profile, dict, is_wide_row)?;

        if enc.encoding == Encoding::Dict {
            used_dict_encoding = true;
        } else if enc.encoding != Encoding::ConstNull && enc.encoding != Encoding::Const {
            has_non_dict_data_block = true;
        }
        null_count_total = null_count_total.saturating_add(u32::from(enc.null_count));

        let (lz4_pct_cap, min_savings_pct) = if profile == WriteProfile::L1Compact {
            match enc.encoding {
                Encoding::ForBitpack
                | Encoding::DeltaBitpack
                | Encoding::BitpackBool
                | Encoding::Dict => (112, 12),
                Encoding::ShreddedMap => (
                    if enc.raw_body.len() >= 8 * 1024 {
                        108
                    } else {
                        0
                    },
                    0,
                ),
                Encoding::ListEncoded | Encoding::EventsStruct | Encoding::LinksStruct => (108, 0),
                Encoding::Plain
                    if matches!(dt, DataType::MapUtf8) && enc.raw_body.len() >= 8 * 1024 =>
                {
                    (108, 0)
                }
                Encoding::Plain
                    if matches!(
                        dt,
                        DataType::Bool
                            | DataType::I64
                            | DataType::F64
                            | DataType::TimestampNs { .. }
                            | DataType::Date
                            | DataType::Decimal128 { .. }
                            | DataType::Interval
                            | DataType::FixedBytes8
                            | DataType::FixedBytes16
                    ) =>
                {
                    (0, 12)
                }
                _ => (0, 0),
            }
        } else if matches!(enc.encoding, Encoding::ForBitpack | Encoding::BitpackBool)
            || (enc.encoding == Encoding::Plain
                && matches!(
                    dt,
                    DataType::Bool
                        | DataType::I64
                        | DataType::F64
                        | DataType::TimestampNs { .. }
                        | DataType::Date
                        | DataType::Decimal128 { .. }
                        | DataType::Interval
                        | DataType::FixedBytes8
                        | DataType::FixedBytes16
                ))
        {
            (0, 12)
        } else {
            (0, 0)
        };
        let entry = enc.write_framed_entry_adaptive(
            mb_first_row,
            mb_len as u16,
            base_offset,
            zstd_level,
            lz4_pct_cap,
            min_savings_pct,
            out_buf,
        )?;
        out_blocks.push(entry);

        if let Some(list) = out_trigrams.as_deref_mut() {
            let filter = if enc.encoding != Encoding::Dict
                && enc.encoding != Encoding::Const
                && enc.encoding != Encoding::ConstNull
            {
                build_narrow_trigram_for_mb(sliced, entry.stored_len as usize)
            } else {
                None
            };
            list.push(filter);
        }

        row_off += mb_len;
    }

    Ok((
        used_dict_encoding,
        has_non_dict_data_block,
        null_count_total,
    ))
}

#[allow(clippy::too_many_arguments)]
fn encode_shredded_map_to_local(
    sm: &ShreddedMapDir,
    is_metadata: bool,
    col: &Column,
    base_row_off: usize,
    rg_rows: usize,
    first_row_u32: u32,
    mb_size: usize,
    profile: WriteProfile,
    zstd_level: i32,
) -> Result<EncodedShreddedMap, StatamError> {
    let logical_col = sm.logical_col_idx as usize;
    let promoted_keys: Vec<String> = sm.keys.iter().map(|k| k.key.clone()).collect();
    let sub_col_indices: Vec<usize> = sm.keys.iter().map(|k| k.sub_col_idx as usize).collect();

    let mut local_core_buf = Vec::new();
    let mut sub_blocks = vec![Vec::new(); sub_col_indices.len()];
    let mut residual_counts = vec![0u32; sub_col_indices.len()];
    let mut res_null_count_total = 0u32;
    let mut local_wide_buf = Vec::new();
    let mut residual_blocks = Vec::new();

    let mut row_off = 0usize;
    while row_off < rg_rows {
        let mb_len = (rg_rows - row_off).min(mb_size);
        let mb_first_row = first_row_u32 + row_off as u32;
        let abs_off = base_row_off + row_off;
        let sliced_owned;
        let sliced = if abs_off == 0 && mb_len == col.len() {
            col
        } else {
            sliced_owned = col.slice(abs_off, mb_len, None)?;
            &sliced_owned
        };
        let flat = as_flat_cow(sliced)?;
        let shredded = shred_map(&flat, &promoted_keys)?;

        for (k_idx, _) in sub_col_indices.iter().enumerate() {
            residual_counts[k_idx] =
                residual_counts[k_idx].saturating_add(shredded.residual_counts[k_idx]);
            let (sub_codec, sub_raw) =
                encode_shredded_subcol_with_codec(&shredded.sub_cols[k_idx], profile)?;
            let blk_off = local_core_buf.len() as u64;
            let info = if profile == WriteProfile::L1Compact && sub_codec == Codec::Zstd {
                write_framed_block_adaptive(
                    Encoding::ShreddedMap,
                    &sub_raw,
                    sub_codec,
                    zstd_level,
                    114,
                    0,
                    &mut local_core_buf,
                )?
            } else {
                write_framed_block(
                    Encoding::ShreddedMap,
                    &sub_raw,
                    sub_codec,
                    zstd_level,
                    &mut local_core_buf,
                )?
            };
            let absent_count =
                u16::try_from(mb_len.saturating_sub(shredded.sub_cols[k_idx].in_map.count_ones()))
                    .unwrap_or(u16::MAX);
            sub_blocks[k_idx].push(BlockEntry {
                first_row: mb_first_row,
                row_count: mb_len as u16,
                null_count: absent_count,
                offset: blk_off,
                stored_len: info.stored_len,
                raw_len: info.raw_len,
                encoding: Encoding::ShreddedMap,
                codec: info.codec,
                block_flags: 0,
                min_inline_len: 0,
                max_inline_len: 0,
                reserved: [0; 3],
                min_inline: [0; 16],
                max_inline: [0; 16],
            });
        }

        let res_enc = crate::codec::encode_shredded_residual_block(&shredded.residual, profile)?;
        res_null_count_total = res_null_count_total.saturating_add(u32::from(res_enc.null_count));
        let res_lz4_cap =
            if profile == WriteProfile::L1Compact && res_enc.raw_body.len() >= 8 * 1024 {
                108
            } else {
                0
            };
        let res_entry = res_enc.write_framed_entry_adaptive(
            mb_first_row,
            mb_len as u16,
            0,
            zstd_level,
            res_lz4_cap,
            0,
            &mut local_wide_buf,
        )?;
        residual_blocks.push(res_entry);

        row_off += mb_len;
    }

    Ok(EncodedShreddedMap {
        logical_col,
        is_metadata,
        local_core_buf,
        sub_col_indices,
        sub_blocks,
        residual_counts,
        res_null_count_total,
        local_wide_buf,
        residual_blocks,
    })
}

#[inline]
fn payload_refs_block_codec(
    profile: WriteProfile,
    preferred_codec: Codec,
    raw_len: usize,
) -> Codec {
    if profile == WriteProfile::L0Fast {
        if raw_len < 1024 {
            Codec::Lz4
        } else {
            default_l1_codec()
        }
    } else {
        preferred_codec
    }
}

fn collect_column_bloom_hashes(col: &Column, out: &mut Vec<u64>) -> Result<(), StatamError> {
    let n = col.len();
    if n == 0 {
        return Ok(());
    }
    match col {
        Column::Const { value, .. } => {
            if let Some(h) = scalar_bloom_hash(value) {
                out.push(h);
            }
        }
        Column::Dict(d) => {
            let flat_vals = d.values();
            let mut seen = vec![false; flat_vals.len()];
            for (r, &c) in d.codes().iter().enumerate() {
                if !d.is_null(r)
                    && let Some(slot) = seen.get_mut(c as usize)
                    && !*slot
                {
                    *slot = true;
                    if let Some(h) = flat_row_bloom_hash(flat_vals, c as usize) {
                        out.push(h);
                    }
                }
            }
        }
        Column::Rle(r) => {
            for run_idx in 0..r.values().len() {
                if let Some(h) = flat_row_bloom_hash(r.values(), run_idx) {
                    out.push(h);
                }
            }
        }
        Column::Flat(f) => {
            let mut prev_h: Option<u64> = None;
            for row in 0..n {
                if let Some(h) = flat_row_bloom_hash(f, row)
                    && Some(h) != prev_h
                {
                    out.push(h);
                    prev_h = Some(h);
                }
            }
        }
    }
    Ok(())
}

fn scalar_bloom_hash(val: &ScalarValue) -> Option<u64> {
    match val {
        ScalarValue::Null(_) => None,
        ScalarValue::FixedBytes16(b) => Some(statam_bloom_hash(b)),
        ScalarValue::FixedBytes8(b) => Some(statam_bloom_hash(b)),
        ScalarValue::Utf8(s) => Some(statam_bloom_hash(s.as_bytes())),
        ScalarValue::Binary(b) => Some(statam_bloom_hash(b)),
        ScalarValue::I64(v) | ScalarValue::TimestampNs { value: v, .. } => {
            Some(statam_bloom_hash(&v.to_le_bytes()))
        }
        ScalarValue::Date(v) => Some(statam_bloom_hash(&v.to_le_bytes())),
        ScalarValue::Decimal128 { value: v, .. } => Some(statam_bloom_hash(&v.to_le_bytes())),
        _ => None,
    }
}

fn insert_flat_bloom_range(flat: &FlatColumn, start: usize, end: usize, sbbf: &mut Sbbf) -> bool {
    if start >= end {
        return false;
    }
    let nulls = flat.nulls();
    match flat.data() {
        FlatData::FixedBytes16(v) => match nulls {
            None => {
                for item in &v[start..end] {
                    sbbf.insert_hash(statam_bloom_hash(item));
                }
                true
            }
            Some(bm) => {
                let mut had = false;
                for (i, item) in v[start..end].iter().enumerate() {
                    if bm.get(start + i) {
                        sbbf.insert_hash(statam_bloom_hash(item));
                        had = true;
                    }
                }
                had
            }
        },
        FlatData::FixedBytes8(v) => match nulls {
            None => {
                for item in &v[start..end] {
                    sbbf.insert_hash(statam_bloom_hash(item));
                }
                true
            }
            Some(bm) => {
                let mut had = false;
                for (i, item) in v[start..end].iter().enumerate() {
                    if bm.get(start + i) {
                        sbbf.insert_hash(statam_bloom_hash(item));
                        had = true;
                    }
                }
                had
            }
        },
        FlatData::Utf8(u) => {
            let offs = u.offsets();
            let bytes = u.bytes();
            match nulls {
                None => {
                    for r in start..end {
                        let s = offs[r] as usize;
                        let e = offs[r + 1] as usize;
                        sbbf.insert_hash(statam_bloom_hash(&bytes[s..e]));
                    }
                    true
                }
                Some(bm) => {
                    let mut had = false;
                    for r in start..end {
                        if bm.get(r) {
                            let s = offs[r] as usize;
                            let e = offs[r + 1] as usize;
                            sbbf.insert_hash(statam_bloom_hash(&bytes[s..e]));
                            had = true;
                        }
                    }
                    had
                }
            }
        }
        FlatData::Binary(b) => {
            let offs = b.offsets();
            let bytes = b.bytes();
            match nulls {
                None => {
                    for r in start..end {
                        let s = offs[r] as usize;
                        let e = offs[r + 1] as usize;
                        sbbf.insert_hash(statam_bloom_hash(&bytes[s..e]));
                    }
                    true
                }
                Some(bm) => {
                    let mut had = false;
                    for r in start..end {
                        if bm.get(r) {
                            let s = offs[r] as usize;
                            let e = offs[r + 1] as usize;
                            sbbf.insert_hash(statam_bloom_hash(&bytes[s..e]));
                            had = true;
                        }
                    }
                    had
                }
            }
        }
        FlatData::I64(v) | FlatData::TimestampNs { values: v, .. } => match nulls {
            None => {
                for &x in &v[start..end] {
                    sbbf.insert_hash(statam_bloom_hash(&x.to_le_bytes()));
                }
                true
            }
            Some(bm) => {
                let mut had = false;
                for (i, &x) in v[start..end].iter().enumerate() {
                    if bm.get(start + i) {
                        sbbf.insert_hash(statam_bloom_hash(&x.to_le_bytes()));
                        had = true;
                    }
                }
                had
            }
        },
        FlatData::Date(v) => match nulls {
            None => {
                for &x in &v[start..end] {
                    sbbf.insert_hash(statam_bloom_hash(&x.to_le_bytes()));
                }
                true
            }
            Some(bm) => {
                let mut had = false;
                for (i, &x) in v[start..end].iter().enumerate() {
                    if bm.get(start + i) {
                        sbbf.insert_hash(statam_bloom_hash(&x.to_le_bytes()));
                        had = true;
                    }
                }
                had
            }
        },
        FlatData::Decimal128 { values, .. } => match nulls {
            None => {
                for &x in &values[start..end] {
                    sbbf.insert_hash(statam_bloom_hash(&x.to_le_bytes()));
                }
                true
            }
            Some(bm) => {
                let mut had = false;
                for (i, &x) in values[start..end].iter().enumerate() {
                    if bm.get(start + i) {
                        sbbf.insert_hash(statam_bloom_hash(&x.to_le_bytes()));
                        had = true;
                    }
                }
                had
            }
        },
        _ => false,
    }
}

fn flat_row_bloom_hash(flat: &FlatColumn, row: usize) -> Option<u64> {
    if flat.is_null(row) {
        return None;
    }
    match flat.data() {
        FlatData::FixedBytes16(v) => Some(statam_bloom_hash(&v[row])),
        FlatData::FixedBytes8(v) => Some(statam_bloom_hash(&v[row])),
        FlatData::Utf8(u) => crate::codec::utf8_bytes_at(u, row).map(statam_bloom_hash),
        FlatData::Binary(b) => b.get(row).map(statam_bloom_hash),
        FlatData::I64(v) | FlatData::TimestampNs { values: v, .. } => {
            Some(statam_bloom_hash(&v[row].to_le_bytes()))
        }
        FlatData::Date(v) => Some(statam_bloom_hash(&v[row].to_le_bytes())),
        FlatData::Decimal128 { values, .. } => Some(statam_bloom_hash(&values[row].to_le_bytes())),
        _ => None,
    }
}

fn build_narrow_trigram_for_mb(col: &Column, stored_len: usize) -> Option<Vec<u8>> {
    if stored_len < MIN_NARROW_TRIGRAM_STORED_BYTES || col.is_empty() {
        return None;
    }
    let flat = as_flat_cow(col).ok()?;
    let FlatData::Utf8(u) = flat.data() else {
        return None;
    };

    let max_bytes = (stored_len / 10).min(MAX_TRIGRAM_BLOOM_BYTES);
    if max_bytes < MIN_TRIGRAM_BLOOM_BYTES {
        return None;
    }
    let cap_pow2 = 1usize << (usize::BITS as usize - 1 - max_bytes.leading_zeros() as usize);

    let mut bloom = TrigramBloom::new(cap_pow2);
    let mut any_trigram = false;
    let nulls = flat.nulls();
    for r in 0..flat.len() {
        if nulls.is_none_or(|bm| bm.get(r))
            && let Some(b) = crate::codec::utf8_bytes_at(u, r)
            && b.len() >= 3
        {
            any_trigram = true;
            bloom.insert_bytes_ilike_folded(b);
        }
    }
    if !any_trigram {
        return None;
    }

    while bloom.as_bytes().len() > MIN_TRIGRAM_BLOOM_BYTES && bloom.bit_density() < 0.25 {
        bloom = bloom.fold_half();
    }
    if bloom.bit_density() > MAX_NARROW_TRIGRAM_DENSITY {
        return None;
    }
    Some(bloom.as_bytes().to_vec())
}

fn write_encoded_payload_refs_block(
    encoded: &EncodedPayloadRefs,
    profile: WriteProfile,
    preferred_codec: Codec,
    zstd_level: i32,
    out: &mut Vec<u8>,
) -> Result<FramedWriteInfo, StatamError> {
    match encoded {
        EncodedPayloadRefs::Subchunked(raw) => {
            write_framed_block(Encoding::PayloadRefs, raw, Codec::None, zstd_level, out)
        }
        EncodedPayloadRefs::PrecompressedZstd { raw_len, stored } => {
            write_precompressed_framed_block(
                Encoding::PayloadRefs,
                Codec::Zstd,
                *raw_len,
                stored,
                out,
            )
        }
        EncodedPayloadRefs::Raw(raw) => {
            let block_codec = payload_refs_block_codec(profile, preferred_codec, raw.len());
            write_framed_block(Encoding::PayloadRefs, raw, block_codec, zstd_level, out)
        }
    }
}
