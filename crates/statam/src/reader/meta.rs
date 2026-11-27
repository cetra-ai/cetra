//! File metadata (`StatamFileMeta`), tail parsing, bounds validation, and lazy index caches.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use cetra_column::{DataType, Schema};

use crate::StatamError;
use crate::codec::dict::FileDict;
use crate::format::{
    BLOCK_ENTRY_LEN, BLOCK_FRAME_HEADER_LEN, BlockEntry, COL_FLAG_BLOOM_IN_HOT_TAIL,
    COL_FLAG_DICT_IN_HOT_TAIL, COL_FLAG_DICT_SORTED, COL_FLAG_HAS_FILE_DICT,
    COL_FLAG_IS_SHREDDED_SUBCOL, COLUMN_DIR_ENTRY_LEN, Codec, ColumnDirEntry, DEFAULT_TAIL_HINT,
    Encoding, FILE_HEADER_LEN, FLAG_HOT_TAIL_LZ4, FileHeader, FileSchemaMeta, LAZY_INDEX_ENTRY_LEN,
    LazyIndexEntry, LazyIndexKind, MAX_BATCH_DECODED_BYTES, MAX_HOT_TAIL_BYTES, POSTSCRIPT_LEN,
    Postscript, ROW_GROUP_ENTRY_LEN, RowGroupEntry, decode_schema, decompress_checked,
};
use crate::io::RangeReader;
use crate::payload::pack::ChunkPackReader;
use crate::simd::bloom::BloomIndex;
use crate::simd::trigram::TrigramBloom;
use crate::writer::NARROW_TRIGRAM_MAGIC;

use super::plan::fetch_coalesced_blocks;
use super::{ReaderCaches, StatamReader};

/// Parsed Hot Tail metadata for a `.statam` file (opened in 1 GET).
#[derive(Clone)]
pub struct StatamFileMeta {
    /// Total file byte length.
    pub file_len: u64,
    /// Decoded 112-byte Postscript.
    pub postscript: Postscript,
    /// Decoded Schema, sort key, pack IDs, locality groups, and shredded map metadata.
    pub schema_meta: FileSchemaMeta,
    /// Physical column directory (`num_phys_cols` entries).
    pub col_dir: Vec<ColumnDirEntry>,
    /// Row-group directory (`num_row_groups` entries).
    pub rg_dir: Vec<RowGroupEntry>,
    /// Mini-block directory (`total_blocks` entries).
    pub block_dir: Vec<BlockEntry>,
    /// Raw Hot Bloom bytes from the Hot Tail.
    pub hot_bloom_bytes: Bytes,
    /// Raw Hot Dict bytes from the Hot Tail.
    pub hot_dict_bytes: Bytes,
    /// Lazy Index directory entries (`lazy_dir_len / 24` entries).
    pub lazy_dir: Vec<LazyIndexEntry>,
    /// Absolute byte offset where the stored Hot Tail begins.
    pub hot_tail_start: u64,
    /// Lazy index sections already captured inside the initial tail prefetch, keyed by
    /// index into `lazy_dir`.
    pub(crate) inline_lazy: HashMap<usize, Bytes>,
}

impl fmt::Debug for StatamFileMeta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatamFileMeta")
            .field("file_len", &self.file_len)
            .field("row_count", &self.postscript.row_count)
            .field("num_logical_cols", &self.postscript.num_logical_cols)
            .field("num_phys_cols", &self.postscript.num_phys_cols)
            .field("num_row_groups", &self.postscript.num_row_groups)
            .field("total_blocks", &self.postscript.total_blocks)
            .finish()
    }
}

impl StatamFileMeta {
    /// Reads and validates the Hot Tail in **1 GET** (with at most 1 follow-up read if `hot_tail_len > tail_hint`).
    pub fn read_from(
        reader: &dyn RangeReader,
        tail_hint: Option<u32>,
    ) -> Result<Self, StatamError> {
        let file_len = reader.file_len();
        let min_len = (FILE_HEADER_LEN + POSTSCRIPT_LEN) as u64;
        if file_len < min_len {
            return Err(StatamError::Corrupt {
                reason: "statam file smaller than header + postscript",
            });
        }

        let hint = u64::from(tail_hint.unwrap_or(DEFAULT_TAIL_HINT)).max(POSTSCRIPT_LEN as u64);
        let tail_start = file_len.saturating_sub(hint);
        let expected_tail_len = file_len
            .checked_sub(tail_start)
            .ok_or(StatamError::Corrupt {
                reason: "invalid tail_start",
            })?;
        let tail = reader.read_range(tail_start..file_len)?;
        if tail.len() as u64 != expected_tail_len || tail.len() < POSTSCRIPT_LEN {
            return Err(StatamError::Corrupt {
                reason: "short read on statam postscript",
            });
        }

        let ps = Postscript::decode(&tail[tail.len() - POSTSCRIPT_LEN..])?;
        if ps.num_row_groups > 0 && ps.num_logical_cols == 0 {
            return Err(StatamError::Corrupt {
                reason: "non-empty row groups with zero logical columns",
            });
        }
        let hot_tail_u64 = u64::from(ps.hot_tail_len);
        if hot_tail_u64 > file_len - FILE_HEADER_LEN as u64 {
            return Err(StatamError::Corrupt {
                reason: "hot_tail_len overlaps file header",
            });
        }
        let hot_tail_start = file_len - hot_tail_u64;

        let header_opt = if tail_start == 0 && tail.len() >= FILE_HEADER_LEN {
            let hdr = FileHeader::decode(&tail[0..FILE_HEADER_LEN])?;
            if hdr.flags != ps.flags || hdr.required_features != ps.required_features {
                return Err(StatamError::Corrupt {
                    reason: "file header and postscript flags mismatch",
                });
            }
            Some(hdr)
        } else {
            None
        };

        let stored_body_len = (ps.hot_tail_len as usize) - POSTSCRIPT_LEN;
        let owned_body: Bytes;
        let stored_body: &[u8] = if tail.len() >= ps.hot_tail_len as usize {
            let start = tail.len() - ps.hot_tail_len as usize;
            &tail[start..start + stored_body_len]
        } else {
            let body_end = file_len - POSTSCRIPT_LEN as u64;
            owned_body = reader.read_range(hot_tail_start..body_end)?;
            if owned_body.len() != stored_body_len {
                return Err(StatamError::Corrupt {
                    reason: "short follow-up read on hot tail body",
                });
            }
            &owned_body
        };

        if crc32c::crc32c(stored_body) != ps.hot_tail_crc32c {
            return Err(StatamError::ChecksumMismatch {
                context: "hot tail",
            });
        }

        let hot_codec = if ps.flags & FLAG_HOT_TAIL_LZ4 != 0 {
            Codec::Lz4
        } else {
            Codec::None
        };
        let raw_body = decompress_checked(
            stored_body,
            hot_codec,
            ps.hot_tail_raw_len as usize,
            MAX_HOT_TAIL_BYTES,
        )?;

        let schema_slice =
            &raw_body[ps.schema_off as usize..ps.schema_off as usize + ps.schema_len as usize];
        let schema_meta = decode_schema(schema_slice)?;
        if schema_meta.schema.len() != ps.num_logical_cols as usize {
            return Err(StatamError::Corrupt {
                reason: "schema logical field count mismatch with postscript",
            });
        }
        if let Some(hdr) = header_opt
            && hdr.table_kind != schema_meta.table_kind
        {
            return Err(StatamError::Corrupt {
                reason: "file header and schema table_kind mismatch",
            });
        }

        let num_phys_cols = ps.num_phys_cols as usize;
        let mut col_dir = Vec::with_capacity(num_phys_cols);
        for idx in 0..num_phys_cols {
            let off = ps.col_dir_off as usize + idx * COLUMN_DIR_ENTRY_LEN;
            let entry = ColumnDirEntry::decode(&raw_body[off..off + COLUMN_DIR_ENTRY_LEN])?;
            if entry.phys_col_id as usize != idx {
                return Err(StatamError::Corrupt {
                    reason: "column directory phys_col_id out of sequence",
                });
            }
            if (entry.logical_col_id as usize) >= ps.num_logical_cols as usize {
                return Err(StatamError::Corrupt {
                    reason: "column directory logical_col_id out of bounds",
                });
            }
            if idx < ps.num_logical_cols as usize && entry.logical_col_id as usize != idx {
                return Err(StatamError::Corrupt {
                    reason: "logical column directory logical_col_id mismatch",
                });
            }
            let blk_end = entry.first_block_idx.checked_add(entry.num_blocks).ok_or(
                StatamError::BoundsExceeded {
                    reason: "column block range overflow",
                },
            )?;
            if blk_end > ps.total_blocks {
                return Err(StatamError::Corrupt {
                    reason: "column block range exceeds total_blocks",
                });
            }
            col_dir.push(entry);
        }

        for sm in &schema_meta.shredded_maps {
            if (sm.logical_col_idx as usize) >= ps.num_logical_cols as usize {
                return Err(StatamError::Corrupt {
                    reason: "shredded map logical_col_idx out of bounds",
                });
            }
            if schema_meta.schema.fields()[sm.logical_col_idx as usize].data_type
                != DataType::MapUtf8
            {
                return Err(StatamError::Corrupt {
                    reason: "shredded map logical_col_idx is not MapUtf8",
                });
            }
            for k in &sm.keys {
                let sidx = k.sub_col_idx as usize;
                if sidx < ps.num_logical_cols as usize || sidx >= num_phys_cols {
                    return Err(StatamError::Corrupt {
                        reason: "shredded map sub_col_idx out of physical column range",
                    });
                }
                if col_dir[sidx].logical_col_id != sm.logical_col_idx {
                    return Err(StatamError::Corrupt {
                        reason: "shredded map sub_col_idx logical_col_id mismatch",
                    });
                }
            }
        }

        let num_row_groups = ps.num_row_groups as usize;
        let mut rg_dir = Vec::with_capacity(num_row_groups);
        let mut expected_first_row = 0u64;
        for idx in 0..num_row_groups {
            let off = ps.rg_dir_off as usize + idx * ROW_GROUP_ENTRY_LEN;
            let rg = RowGroupEntry::decode(&raw_body[off..off + ROW_GROUP_ENTRY_LEN])?;
            if u64::from(rg.first_row) != expected_first_row {
                return Err(StatamError::Corrupt {
                    reason: "row group first_row not contiguous",
                });
            }
            expected_first_row = expected_first_row
                .checked_add(u64::from(rg.row_count))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "row group row_count sum overflow",
                })?;
            validate_rg_bounds(&rg, hot_tail_start)?;
            rg_dir.push(rg);
        }
        if expected_first_row != ps.row_count {
            return Err(StatamError::Corrupt {
                reason: "row group row_counts do not sum to postscript row_count",
            });
        }

        let total_blocks = ps.total_blocks as usize;
        let mut block_dir = Vec::with_capacity(total_blocks);
        for idx in 0..total_blocks {
            let off = ps.block_dir_off as usize + idx * BLOCK_ENTRY_LEN;
            let blk = BlockEntry::decode(&raw_body[off..off + BLOCK_ENTRY_LEN])?;
            if blk.stored_len > 0 {
                let total_blk = (BLOCK_FRAME_HEADER_LEN as u64)
                    .checked_add(u64::from(blk.stored_len))
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "block byte span overflow",
                    })?;
                let blk_end =
                    blk.offset
                        .checked_add(total_blk)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "block end offset overflow",
                        })?;
                if blk.offset < FILE_HEADER_LEN as u64 || blk_end > hot_tail_start {
                    return Err(StatamError::Corrupt {
                        reason: "block offset lies outside file data region",
                    });
                }
            }
            block_dir.push(blk);
        }

        // Validate every physical column's blocks partition each RG's exact row range
        // with identical mini-block count and identical (first_row, row_count), and every
        // non-inline block lies within its owning RG's [core_offset .. wide_offset + wide_len].
        let mut cursors = vec![0usize; num_phys_cols];
        for rg in &rg_dir {
            let rg_start = u64::from(rg.first_row);
            let rg_end = rg_start.checked_add(u64::from(rg.row_count)).ok_or(
                StatamError::BoundsExceeded {
                    reason: "row group row end overflow",
                },
            )?;
            let rg_wide_end = rg.wide_offset.checked_add(u64::from(rg.wide_len)).ok_or(
                StatamError::BoundsExceeded {
                    reason: "row group wide end overflow",
                },
            )?;

            if num_phys_cols == 0 {
                continue;
            }

            let mut expected_mbs: Vec<(u32, u16)> = Vec::new();
            {
                let cd0 = &col_dir[0];
                let col0_start = cd0.first_block_idx as usize;
                let col0_end = col0_start + cd0.num_blocks as usize;
                let mut cursor = col0_start + cursors[0];
                let mut expected_row = rg_start;
                while expected_row < rg_end {
                    if cursor >= col0_end {
                        return Err(StatamError::Corrupt {
                            reason: "column 0 blocks do not cover row group row range",
                        });
                    }
                    let blk = &block_dir[cursor];
                    if blk.row_count == 0 || blk.null_count > blk.row_count {
                        return Err(StatamError::Corrupt {
                            reason: "invalid mini-block row_count or null_count",
                        });
                    }
                    if u64::from(blk.first_row) != expected_row {
                        return Err(StatamError::Corrupt {
                            reason: "mini-block first_row not contiguous within row group",
                        });
                    }
                    let next_row = expected_row + u64::from(blk.row_count);
                    if next_row > rg_end {
                        return Err(StatamError::Corrupt {
                            reason: "mini-block extends past row group row range",
                        });
                    }
                    validate_block_in_rg(blk, rg, rg_wide_end)?;
                    expected_mbs.push((blk.first_row, blk.row_count));
                    expected_row = next_row;
                    cursor += 1;
                    cursors[0] += 1;
                }
            }

            for (pidx, cd) in col_dir.iter().enumerate().skip(1) {
                let col_start = cd.first_block_idx as usize;
                let col_end = col_start + cd.num_blocks as usize;
                let start_cursor = col_start + cursors[pidx];
                for (cursor, &(exp_first, exp_count)) in (start_cursor..).zip(expected_mbs.iter()) {
                    if cursor >= col_end {
                        return Err(StatamError::Corrupt {
                            reason: "physical column missing mini-blocks for row group",
                        });
                    }
                    let blk = &block_dir[cursor];
                    if blk.first_row != exp_first
                        || blk.row_count != exp_count
                        || blk.null_count > blk.row_count
                    {
                        return Err(StatamError::Corrupt {
                            reason: "physical column mini-block layout mismatch in row group",
                        });
                    }
                    validate_block_in_rg(blk, rg, rg_wide_end)?;
                    cursors[pidx] += 1;
                }
            }
        }
        for (pidx, cd) in col_dir.iter().enumerate() {
            if cursors[pidx] != cd.num_blocks as usize {
                return Err(StatamError::Corrupt {
                    reason: "column directory has blocks outside row groups",
                });
            }
        }

        let hot_bloom_bytes = Bytes::copy_from_slice(
            &raw_body
                [ps.hot_bloom_off as usize..ps.hot_bloom_off as usize + ps.hot_bloom_len as usize],
        );
        let hot_dict_bytes = Bytes::copy_from_slice(
            &raw_body
                [ps.hot_dict_off as usize..ps.hot_dict_off as usize + ps.hot_dict_len as usize],
        );

        let lazy_count = (ps.lazy_dir_len as usize) / LAZY_INDEX_ENTRY_LEN;
        let mut lazy_dir = Vec::with_capacity(lazy_count);
        let mut inline_lazy: HashMap<usize, Bytes> = HashMap::new();
        let mut seen_lazy_keys: HashSet<(u8, u32)> = HashSet::new();
        let mut inline_lazy_bytes = 0usize;
        for idx in 0..lazy_count {
            let off = ps.lazy_dir_off as usize + idx * LAZY_INDEX_ENTRY_LEN;
            let entry = LazyIndexEntry::decode(&raw_body[off..off + LAZY_INDEX_ENTRY_LEN])?;
            if (entry.phys_col_id as usize) >= num_phys_cols {
                return Err(StatamError::Corrupt {
                    reason: "lazy index phys_col_id out of bounds",
                });
            }
            let end = entry.offset.checked_add(u64::from(entry.len)).ok_or(
                StatamError::BoundsExceeded {
                    reason: "lazy index end offset overflow",
                },
            )?;
            if entry.offset < FILE_HEADER_LEN as u64 || end > hot_tail_start {
                return Err(StatamError::Corrupt {
                    reason: "lazy index section lies outside file data region",
                });
            }
            let is_first_for_key = seen_lazy_keys.insert((entry.kind as u8, entry.phys_col_id));
            if is_first_for_key
                && entry.offset >= tail_start
                && end <= hot_tail_start
                && let Ok(rel_s) = usize::try_from(entry.offset - tail_start)
                && let Some(rel_e) = rel_s.checked_add(entry.len as usize)
                && let Some(slice) = tail.get(rel_s..rel_e)
                && let Some(next_total) = inline_lazy_bytes.checked_add(slice.len())
                && next_total <= tail.len()
            {
                inline_lazy_bytes = next_total;
                inline_lazy.insert(idx, Bytes::copy_from_slice(slice));
            }
            lazy_dir.push(entry);
        }

        for cd in &col_dir {
            if cd.col_flags & COL_FLAG_BLOOM_IN_HOT_TAIL != 0 && cd.bloom_len > 0 {
                let end = cd.bloom_offset.checked_add(u64::from(cd.bloom_len)).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "hot bloom offset + len overflow",
                    },
                )?;
                if end > hot_bloom_bytes.len() as u64 {
                    return Err(StatamError::Corrupt {
                        reason: "column hot bloom out of bounds",
                    });
                }
            }
            if cd.col_flags & COL_FLAG_DICT_IN_HOT_TAIL != 0 && cd.dict_len > 0 {
                let end = cd.dict_offset.checked_add(u64::from(cd.dict_len)).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "hot dict offset + len overflow",
                    },
                )?;
                if end > hot_dict_bytes.len() as u64 {
                    return Err(StatamError::Corrupt {
                        reason: "column hot dict out of bounds",
                    });
                }
            }
        }

        Ok(Self {
            file_len,
            postscript: ps,
            schema_meta,
            col_dir,
            rg_dir,
            block_dir,
            hot_bloom_bytes,
            hot_dict_bytes,
            lazy_dir,
            hot_tail_start,
            inline_lazy,
        })
    }

    /// Returns the logical [`Schema`].
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema_meta.schema
    }

    /// Total row count in the file.
    pub fn row_count(&self) -> u64 {
        self.postscript.row_count
    }

    /// Number of PAX Row Groups.
    pub fn num_row_groups(&self) -> usize {
        self.rg_dir.len()
    }

    /// Decodes a Hot-Tail-resident SBBF bloom index (`BF01`/`BF02`) for physical column `pidx`, if present.
    pub fn hot_bloom(&self, pidx: usize) -> Result<Option<BloomIndex>, StatamError> {
        let Some(cd) = self.col_dir.get(pidx) else {
            return Ok(None);
        };
        if cd.bloom_len == 0 || (cd.col_flags & COL_FLAG_BLOOM_IN_HOT_TAIL) == 0 {
            return Ok(None);
        }
        let start = cd.bloom_offset as usize;
        let end = start + cd.bloom_len as usize;
        let bloom = BloomIndex::from_bytes(&self.hot_bloom_bytes[start..end])?;
        if let BloomIndex::PerRowGroup(rgs) = &bloom
            && rgs.len() != self.rg_dir.len()
        {
            return Err(StatamError::Corrupt {
                reason: "per-RG bloom count does not match num_row_groups",
            });
        }
        Ok(Some(bloom))
    }

    fn expected_dict_data_type(&self, pidx: usize) -> Result<DataType, StatamError> {
        let cd = self.col_dir.get(pidx).ok_or(StatamError::Corrupt {
            reason: "physical column index out of bounds",
        })?;
        if (cd.col_flags & COL_FLAG_IS_SHREDDED_SUBCOL) != 0 {
            Ok(DataType::Utf8)
        } else {
            self.schema_meta
                .schema
                .fields()
                .get(cd.logical_col_id as usize)
                .map(|f| f.data_type.clone())
                .ok_or(StatamError::Corrupt {
                    reason: "logical column index out of bounds",
                })
        }
    }

    /// Decodes a Hot-Tail-resident `FileDict` for physical column `pidx`, if present.
    pub fn hot_dict(&self, pidx: usize) -> Result<Option<FileDict>, StatamError> {
        let Some(cd) = self.col_dir.get(pidx) else {
            return Ok(None);
        };
        if cd.dict_len == 0
            || (cd.col_flags & COL_FLAG_HAS_FILE_DICT) == 0
            || (cd.col_flags & COL_FLAG_DICT_IN_HOT_TAIL) == 0
        {
            return Ok(None);
        }
        let start = cd.dict_offset as usize;
        let end = start + cd.dict_len as usize;
        let sorted = (cd.col_flags & COL_FLAG_DICT_SORTED) != 0;
        let dict = FileDict::decode(&self.hot_dict_bytes[start..end], sorted)?;
        let expected_dt = self.expected_dict_data_type(pidx)?;
        if dict.values().data_type() != expected_dt {
            return Err(StatamError::Corrupt {
                reason: "dictionary data type does not match column schema",
            });
        }
        Ok(Some(dict))
    }

    /// Returns the `BlockEntry` slice belonging to Row Group `rg_idx` for physical column `pidx`.
    pub(crate) fn rg_blocks(&self, pidx: usize, rg_idx: usize) -> &[BlockEntry] {
        let cd = &self.col_dir[pidx];
        let rg = &self.rg_dir[rg_idx];
        let col_blocks = &self.block_dir
            [cd.first_block_idx as usize..(cd.first_block_idx + cd.num_blocks) as usize];
        let rg_start = rg.first_row;
        let rg_end = rg.first_row + rg.row_count;
        let lo = col_blocks.partition_point(|b| b.first_row < rg_start);
        let hi = col_blocks.partition_point(|b| b.first_row < rg_end);
        &col_blocks[lo..hi]
    }
}

fn validate_rg_bounds(rg: &RowGroupEntry, hot_tail_start: u64) -> Result<(), StatamError> {
    if rg.core_len as usize > MAX_BATCH_DECODED_BYTES
        || rg.wide_len as usize > MAX_BATCH_DECODED_BYTES
        || rg.l0_payload_len as usize > MAX_BATCH_DECODED_BYTES
    {
        return Err(StatamError::BoundsExceeded {
            reason: "row group region exceeds MAX_BATCH_DECODED_BYTES",
        });
    }
    if rg.core_offset < FILE_HEADER_LEN as u64 {
        return Err(StatamError::Corrupt {
            reason: "row group core_offset overlaps file header",
        });
    }
    let core_end =
        rg.core_offset
            .checked_add(u64::from(rg.core_len))
            .ok_or(StatamError::BoundsExceeded {
                reason: "row group core_end overflow",
            })?;
    if rg.wide_offset != core_end {
        return Err(StatamError::Corrupt {
            reason: "row group wide_offset not contiguous with core_offset + core_len",
        });
    }
    let wide_end =
        rg.wide_offset
            .checked_add(u64::from(rg.wide_len))
            .ok_or(StatamError::BoundsExceeded {
                reason: "row group wide end overflow",
            })?;
    if wide_end > hot_tail_start {
        return Err(StatamError::Corrupt {
            reason: "row group wide region overlaps hot tail",
        });
    }
    if rg.l0_payload_len > 0 {
        if rg.l0_payload_offset != wide_end {
            return Err(StatamError::Corrupt {
                reason: "row group l0_payload_offset not contiguous with wide_offset + wide_len",
            });
        }
        let l0_end = rg
            .l0_payload_offset
            .checked_add(u64::from(rg.l0_payload_len))
            .ok_or(StatamError::BoundsExceeded {
                reason: "row group l0_payload end overflow",
            })?;
        if l0_end > hot_tail_start {
            return Err(StatamError::Corrupt {
                reason: "row group l0_payload overlaps hot tail",
            });
        }
    }
    let total_end = core_end
        .checked_add(u64::from(rg.wide_len))
        .and_then(|v| v.checked_add(u64::from(rg.l0_payload_len)))
        .ok_or(StatamError::BoundsExceeded {
            reason: "row group total extent overflow",
        })?;
    if total_end > hot_tail_start {
        return Err(StatamError::Corrupt {
            reason: "row group total extent overlaps hot tail",
        });
    }
    Ok(())
}

fn validate_block_in_rg(
    blk: &BlockEntry,
    rg: &RowGroupEntry,
    rg_wide_end: u64,
) -> Result<(), StatamError> {
    let is_inline = matches!(blk.encoding, Encoding::Const | Encoding::ConstNull);
    if blk.stored_len > 0 || !is_inline {
        let total_blk = (BLOCK_FRAME_HEADER_LEN as u64)
            .checked_add(u64::from(blk.stored_len))
            .ok_or(StatamError::BoundsExceeded {
                reason: "block byte span overflow",
            })?;
        let blk_end = blk
            .offset
            .checked_add(total_blk)
            .ok_or(StatamError::BoundsExceeded {
                reason: "block end offset overflow",
            })?;
        if blk.offset < rg.core_offset || blk_end > rg_wide_end {
            return Err(StatamError::Corrupt {
                reason: "non-inline block lies outside owning row group bounds",
            });
        }
    }
    Ok(())
}

impl StatamReader {
    fn decode_and_cache_lazy_entry(
        &self,
        guard: &mut ReaderCaches,
        entry: LazyIndexEntry,
        bytes: &[u8],
    ) -> Result<(), StatamError> {
        let pidx = entry.phys_col_id as usize;
        match entry.kind {
            LazyIndexKind::Bloom => {
                if bytes.len() != entry.len as usize {
                    return Err(StatamError::Corrupt {
                        reason: "short read on lazy bloom filter",
                    });
                }
                if crc32c::crc32c(bytes) != entry.crc32c {
                    return Err(StatamError::ChecksumMismatch {
                        context: "lazy bloom filter",
                    });
                }
                let bloom = BloomIndex::from_bytes(bytes)?;
                if let BloomIndex::PerRowGroup(rgs) = &bloom
                    && rgs.len() != self.meta.rg_dir.len()
                {
                    return Err(StatamError::Corrupt {
                        reason: "per-RG bloom count does not match num_row_groups",
                    });
                }
                guard.blooms.insert(pidx, bloom);
            }
            LazyIndexKind::NarrowTrigram => {
                if bytes.len() != entry.len as usize {
                    return Err(StatamError::Corrupt {
                        reason: "short read on narrow trigram index",
                    });
                }
                if crc32c::crc32c(bytes) != entry.crc32c {
                    return Err(StatamError::ChecksumMismatch {
                        context: "narrow trigram index",
                    });
                }
                let blooms = decode_narrow_trigram_section(
                    bytes,
                    self.meta.col_dir[pidx].num_blocks as usize,
                )?;
                guard.narrow_trigrams.insert(pidx, blooms);
            }
            LazyIndexKind::Dict => {
                if bytes.len() != entry.len as usize {
                    return Err(StatamError::Corrupt {
                        reason: "short read on lazy dictionary",
                    });
                }
                if crc32c::crc32c(bytes) != entry.crc32c {
                    return Err(StatamError::ChecksumMismatch {
                        context: "lazy dictionary",
                    });
                }
                let sorted = (self.meta.col_dir[pidx].col_flags & COL_FLAG_DICT_SORTED) != 0;
                let dict = Arc::new(FileDict::decode(bytes, sorted)?);
                let expected_dt = self.meta.expected_dict_data_type(pidx)?;
                if dict.values().data_type() != expected_dt {
                    return Err(StatamError::Corrupt {
                        reason: "dictionary data type does not match column schema",
                    });
                }
                guard.dicts.insert(pidx, dict);
            }
        }
        Ok(())
    }

    /// Populates `self.caches` from any matching `inline_lazy` slices with 0 I/O, and returns
    /// the remaining uncached [`LazyIndexEntry`] items that still require remote reads.
    fn collect_missing_lazy_entries(
        &self,
        bloom_cols: &[usize],
        trigram_cols: &[usize],
        dict_cols: &[usize],
    ) -> Result<Vec<LazyIndexEntry>, StatamError> {
        let mut missing: Vec<LazyIndexEntry> = Vec::new();
        let mut guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());

        for &cidx in bloom_cols {
            let cd = &self.meta.col_dir[cidx];
            if cd.bloom_len > 0
                && !guard.blooms.contains_key(&cidx)
                && !missing
                    .iter()
                    .any(|e| e.kind == LazyIndexKind::Bloom && e.phys_col_id as usize == cidx)
                && let Some((idx, entry)) =
                    self.meta.lazy_dir.iter().enumerate().find(|(_, e)| {
                        e.kind == LazyIndexKind::Bloom && e.phys_col_id as usize == cidx
                    })
            {
                if let Some(inline) = self.meta.inline_lazy.get(&idx) {
                    self.decode_and_cache_lazy_entry(&mut guard, *entry, inline)?;
                } else {
                    missing.push(*entry);
                }
            }
        }

        for &cidx in trigram_cols {
            if !guard.narrow_trigrams.contains_key(&cidx)
                && !missing.iter().any(|e| {
                    e.kind == LazyIndexKind::NarrowTrigram && e.phys_col_id as usize == cidx
                })
                && let Some((idx, entry)) = self.meta.lazy_dir.iter().enumerate().find(|(_, e)| {
                    e.kind == LazyIndexKind::NarrowTrigram && e.phys_col_id as usize == cidx
                })
            {
                if let Some(inline) = self.meta.inline_lazy.get(&idx) {
                    self.decode_and_cache_lazy_entry(&mut guard, *entry, inline)?;
                } else {
                    missing.push(*entry);
                }
            }
        }

        for &pidx in dict_cols {
            let cd = &self.meta.col_dir[pidx];
            if (cd.col_flags & COL_FLAG_HAS_FILE_DICT) != 0
                && !guard.dicts.contains_key(&pidx)
                && !missing
                    .iter()
                    .any(|e| e.kind == LazyIndexKind::Dict && e.phys_col_id as usize == pidx)
                && let Some((idx, entry)) =
                    self.meta.lazy_dir.iter().enumerate().find(|(_, e)| {
                        e.kind == LazyIndexKind::Dict && e.phys_col_id as usize == pidx
                    })
            {
                if let Some(inline) = self.meta.inline_lazy.get(&idx) {
                    self.decode_and_cache_lazy_entry(&mut guard, *entry, inline)?;
                } else {
                    missing.push(*entry);
                }
            }
        }

        Ok(missing)
    }

    /// Decodes any requested lazy indexes that were already prefetched in `inline_lazy` (0 I/O).
    pub(super) fn load_inline_lazy_indexes(
        &self,
        bloom_cols: &[usize],
        trigram_cols: &[usize],
        dict_cols: &[usize],
    ) -> Result<(), StatamError> {
        let _ = self.collect_missing_lazy_entries(bloom_cols, trigram_cols, dict_cols)?;
        Ok(())
    }

    /// Fetches `block_ranges` AND any remaining uncached lazy indexes (`bloom_cols`,
    /// `trigram_cols`, `dict_cols`) together in **ONE** coalesced `reader.read_ranges(...)` wave,
    /// consuming any `inline_lazy` slices first with 0 I/O.
    pub(super) fn fetch_blocks_and_lazy(
        &self,
        block_ranges: &[(u64, u64)],
        bloom_cols: &[usize],
        trigram_cols: &[usize],
        dict_cols: &[usize],
    ) -> Result<HashMap<(u64, u64), Bytes>, StatamError> {
        let missing_lazy =
            self.collect_missing_lazy_entries(bloom_cols, trigram_cols, dict_cols)?;
        if block_ranges.is_empty() && missing_lazy.is_empty() {
            return Ok(HashMap::new());
        }

        let mut all_ranges: Vec<(u64, u64)> =
            Vec::with_capacity(block_ranges.len() + missing_lazy.len());
        all_ranges.extend_from_slice(block_ranges);
        for entry in &missing_lazy {
            let end = entry.offset.checked_add(u64::from(entry.len)).ok_or(
                StatamError::BoundsExceeded {
                    reason: "lazy index end offset overflow",
                },
            )?;
            all_ranges.push((entry.offset, end));
        }

        let fetched = fetch_coalesced_blocks(self.reader.as_ref(), &all_ranges)?;

        if !missing_lazy.is_empty() {
            let mut guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
            for entry in missing_lazy {
                let key = (entry.offset, entry.offset + u64::from(entry.len));
                let bytes = fetched.get(&key).ok_or(StatamError::Corrupt {
                    reason: "missing fetched lazy index section",
                })?;
                self.decode_and_cache_lazy_entry(&mut guard, entry, bytes)?;
            }
        }

        Ok(fetched)
    }

    /// Ensures all requested blooms, narrow trigrams, and lazy dictionaries are cached,
    /// using `inline_lazy` first and fetching any remaining entries in **ONE** `read_ranges` wave.
    pub(super) fn ensure_lazy_indexes_loaded(
        &self,
        bloom_cols: &[usize],
        trigram_cols: &[usize],
        dict_cols: &[usize],
    ) -> Result<(), StatamError> {
        let _ = self.fetch_blocks_and_lazy(&[], bloom_cols, trigram_cols, dict_cols)?;
        Ok(())
    }

    /// Checks already-loaded file-level SBBF blooms against `lookups_with_hashes` (0 I/O).
    pub(super) fn blooms_may_match(&self, lookups_with_hashes: &[(usize, Vec<u64>)]) -> bool {
        let guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
        for (cidx, hashes) in lookups_with_hashes {
            if let Some(bloom) = guard.blooms.get(cidx)
                && !bloom.contains_any_hash(hashes)
            {
                return false;
            }
        }
        true
    }

    pub(super) fn ensure_dicts_loaded(&self, phys_cols: &[usize]) -> Result<(), StatamError> {
        self.ensure_lazy_indexes_loaded(&[], &[], phys_cols)
    }

    pub(super) fn get_cached_dict(&self, pidx: usize) -> Option<Arc<FileDict>> {
        self.caches
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .dicts
            .get(&pidx)
            .map(Arc::clone)
    }

    pub(super) fn resolve_pack(
        &self,
        pack_id: [u8; 16],
    ) -> Result<Arc<ChunkPackReader>, StatamError> {
        {
            let guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(p) = guard.packs.get(&pack_id) {
                return Ok(Arc::clone(p));
            }
        }
        let _init_guard = self
            .pack_init_lock
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        {
            let guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(p) = guard.packs.get(&pack_id) {
                return Ok(Arc::clone(p));
            }
        }
        let resolver = self
            .packs
            .as_ref()
            .ok_or(StatamError::MissingPack { pack_id })?;
        let pack = resolver.resolve(pack_id)?;
        let mut guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
        guard.packs.insert(pack_id, Arc::clone(&pack));
        Ok(pack)
    }
}

fn decode_narrow_trigram_section(
    bytes: &[u8],
    expected_blocks: usize,
) -> Result<Vec<Option<TrigramBloom>>, StatamError> {
    if bytes.len() < 8 || bytes[0..4] != NARROW_TRIGRAM_MAGIC {
        return Err(StatamError::Corrupt {
            reason: "invalid narrow trigram section header",
        });
    }
    let count = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    if count != expected_blocks {
        return Err(StatamError::Corrupt {
            reason: "narrow trigram block count mismatch",
        });
    }
    let table_bytes = count.checked_mul(8).ok_or(StatamError::BoundsExceeded {
        reason: "narrow trigram table overflow",
    })?;
    let body_start = 8usize
        .checked_add(table_bytes)
        .ok_or(StatamError::BoundsExceeded {
            reason: "narrow trigram header overflow",
        })?;
    if body_start > bytes.len() {
        return Err(StatamError::Corrupt {
            reason: "truncated narrow trigram offset table",
        });
    }
    let body = &bytes[body_start..];
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let p = 8 + i * 8;
        let off = u32::from_le_bytes([bytes[p], bytes[p + 1], bytes[p + 2], bytes[p + 3]]) as usize;
        let len =
            u32::from_le_bytes([bytes[p + 4], bytes[p + 5], bytes[p + 6], bytes[p + 7]]) as usize;
        if len == 0 {
            out.push(None);
        } else {
            let end = off.checked_add(len).ok_or(StatamError::BoundsExceeded {
                reason: "narrow trigram filter slice overflow",
            })?;
            let slice = body.get(off..end).ok_or(StatamError::Corrupt {
                reason: "narrow trigram filter out of section bounds",
            })?;
            out.push(Some(TrigramBloom::from_bytes(slice.to_vec())?));
        }
    }
    Ok(out)
}
