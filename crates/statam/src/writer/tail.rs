//! Hot Tail assembly, spill placement, and Lazy Index serialization.

use crate::StatamError;
use crate::format::{
    COL_FLAG_BLOOM_IN_HOT_TAIL, COL_FLAG_DICT_IN_HOT_TAIL, COL_FLAG_HAS_TRIGRAM_INDEX,
    COLUMN_DIR_ENTRY_LEN, ColumnDirEntry, LAZY_INDEX_ENTRY_LEN, LazyIndexEntry, LazyIndexKind,
    MAX_BLOCK_RAW_BYTES, MAX_HOT_TAIL_BYTES, POSTSCRIPT_LEN,
};

/// Compressed Hot Tail soft budget (120 KiB) for keeping small FileDicts and secondary blooms hot
/// within `DEFAULT_TAIL_HINT` (128 KiB).
pub(super) const HOT_TAIL_SOFT_BUDGET_BYTES: usize = 120 * 1024;
/// Hard Hot Tail budget (1 MiB) so large files with >128 KiB directories still keep primary bloom hot.
pub(super) const HOT_TAIL_HARD_BUDGET_BYTES: usize = 1 << 20;
/// Maximum size of a single FileDict kept inside the Hot Tail (8 KiB).
pub(super) const HOT_DICT_MAX_BYTES: usize = 8 * 1024;
/// Magic bytes for a per-block narrow-column trigram section.
pub(crate) const NARROW_TRIGRAM_MAGIC: [u8; 4] = *b"NT01";

pub(super) struct AssembledHotTail {
    pub(super) hot_tail_raw_len: u32,
    pub(super) hot_tail_len: u32,
    pub(super) hot_tail_stored: Vec<u8>,
    pub(super) is_lz4: bool,
    pub(super) lazy_data_buf: Vec<u8>,
    pub(super) cur_file_off: u64,
    pub(super) schema_len: u32,
    pub(super) col_dir_off: u32,
    pub(super) rg_dir_off: u32,
    pub(super) block_dir_off: u32,
    pub(super) hot_bloom_off: u32,
    pub(super) hot_bloom_len: u32,
    pub(super) hot_dict_off: u32,
    pub(super) hot_dict_len: u32,
    pub(super) lazy_dir_off: u32,
    pub(super) lazy_dir_len: u32,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn assemble_hot_tail_and_lazy(
    base_col_dirs: &[ColumnDirEntry],
    schema_bytes: &[u8],
    rg_dir_bytes: &[u8],
    block_dir_bytes: &[u8],
    data_end_offset: u64,
    compress_hot_tail_lz4: bool,
    primary_bloom: Option<&(usize, Vec<u8>, bool)>,
    dict_items: &[(usize, Vec<u8>, bool)],
    sec_blooms: &[(usize, Vec<u8>, bool)],
    trigram_items: &[(usize, Vec<u8>)],
) -> Result<AssembledHotTail, StatamError> {
    let mut col_dirs = base_col_dirs.to_vec();
    let mut hot_bloom_buf = Vec::new();
    let mut hot_dict_buf = Vec::new();
    let mut lazy_data_buf = Vec::new();
    let mut lazy_entries = Vec::new();
    let mut cur_file_off = data_end_offset;

    if let Some((hot_idx, bloom_bytes, in_hot)) = primary_bloom {
        if *in_hot {
            let off = hot_bloom_buf.len() as u64;
            let len = bloom_bytes.len() as u32;
            hot_bloom_buf.extend_from_slice(bloom_bytes);
            col_dirs[*hot_idx].col_flags |= COL_FLAG_BLOOM_IN_HOT_TAIL;
            col_dirs[*hot_idx].bloom_offset = off;
            col_dirs[*hot_idx].bloom_len = len;
        } else {
            append_lazy_entry(
                LazyIndexKind::Bloom,
                *hot_idx as u32,
                bloom_bytes,
                &mut cur_file_off,
                &mut lazy_data_buf,
                &mut lazy_entries,
                &mut col_dirs[*hot_idx],
            )?;
        }
    }

    for (pidx, dict_bytes, in_hot) in dict_items {
        if *in_hot {
            let off = hot_dict_buf.len() as u64;
            let len = dict_bytes.len() as u32;
            hot_dict_buf.extend_from_slice(dict_bytes);
            col_dirs[*pidx].col_flags |= COL_FLAG_DICT_IN_HOT_TAIL;
            col_dirs[*pidx].dict_offset = off;
            col_dirs[*pidx].dict_len = len;
        } else {
            append_lazy_entry(
                LazyIndexKind::Dict,
                *pidx as u32,
                dict_bytes,
                &mut cur_file_off,
                &mut lazy_data_buf,
                &mut lazy_entries,
                &mut col_dirs[*pidx],
            )?;
        }
    }

    for (cidx, bloom_bytes, in_hot) in sec_blooms {
        if *in_hot {
            let off = hot_bloom_buf.len() as u64;
            let len = bloom_bytes.len() as u32;
            hot_bloom_buf.extend_from_slice(bloom_bytes);
            col_dirs[*cidx].col_flags |= COL_FLAG_BLOOM_IN_HOT_TAIL;
            col_dirs[*cidx].bloom_offset = off;
            col_dirs[*cidx].bloom_len = len;
        } else {
            append_lazy_entry(
                LazyIndexKind::Bloom,
                *cidx as u32,
                bloom_bytes,
                &mut cur_file_off,
                &mut lazy_data_buf,
                &mut lazy_entries,
                &mut col_dirs[*cidx],
            )?;
        }
    }

    for (cidx, sec_bytes) in trigram_items {
        append_lazy_entry(
            LazyIndexKind::NarrowTrigram,
            *cidx as u32,
            sec_bytes,
            &mut cur_file_off,
            &mut lazy_data_buf,
            &mut lazy_entries,
            &mut col_dirs[*cidx],
        )?;
        col_dirs[*cidx].col_flags |= COL_FLAG_HAS_TRIGRAM_INDEX;
    }

    let exact_raw_len = schema_bytes.len()
        + col_dirs.len() * COLUMN_DIR_ENTRY_LEN
        + rg_dir_bytes.len()
        + block_dir_bytes.len()
        + hot_bloom_buf.len()
        + hot_dict_buf.len()
        + lazy_entries.len() * LAZY_INDEX_ENTRY_LEN;
    if exact_raw_len > MAX_HOT_TAIL_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "uncompressed hot tail exceeds MAX_HOT_TAIL_BYTES",
        });
    }

    let mut hot_tail_raw = Vec::with_capacity(exact_raw_len);
    let schema_len =
        u32::try_from(schema_bytes.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "schema_len overflows u32",
        })?;
    hot_tail_raw.extend_from_slice(schema_bytes);

    let col_dir_off = hot_tail_raw.len() as u32;
    for cd in &col_dirs {
        hot_tail_raw.extend_from_slice(&cd.encode());
    }

    let rg_dir_off = hot_tail_raw.len() as u32;
    hot_tail_raw.extend_from_slice(rg_dir_bytes);

    let block_dir_off = hot_tail_raw.len() as u32;
    hot_tail_raw.extend_from_slice(block_dir_bytes);

    let (hot_bloom_off, hot_bloom_len) = if hot_bloom_buf.is_empty() {
        (0u32, 0u32)
    } else {
        let off = hot_tail_raw.len() as u32;
        let len = hot_bloom_buf.len() as u32;
        hot_tail_raw.extend_from_slice(&hot_bloom_buf);
        (off, len)
    };

    let (hot_dict_off, hot_dict_len) = if hot_dict_buf.is_empty() {
        (0u32, 0u32)
    } else {
        let off = hot_tail_raw.len() as u32;
        let len = hot_dict_buf.len() as u32;
        hot_tail_raw.extend_from_slice(&hot_dict_buf);
        (off, len)
    };

    let (lazy_dir_off, lazy_dir_len) = if lazy_entries.is_empty() {
        (0u32, 0u32)
    } else {
        let off = hot_tail_raw.len() as u32;
        for entry in &lazy_entries {
            hot_tail_raw.extend_from_slice(&entry.encode());
        }
        let len = (lazy_entries.len() * LAZY_INDEX_ENTRY_LEN) as u32;
        (off, len)
    };

    let hot_tail_raw_len = hot_tail_raw.len() as u32;
    let (hot_tail_stored, is_lz4) = if compress_hot_tail_lz4 {
        let compressed = lz4_flex::block::compress(&hot_tail_raw);
        if compressed.len().saturating_add(16) < hot_tail_raw.len() {
            (compressed, true)
        } else {
            (hot_tail_raw, false)
        }
    } else {
        (hot_tail_raw, false)
    };

    let hot_tail_len = hot_tail_stored
        .len()
        .checked_add(POSTSCRIPT_LEN)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or(StatamError::BoundsExceeded {
            reason: "hot_tail_len overflows u32",
        })?;
    if (hot_tail_len as usize) > MAX_HOT_TAIL_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "stored hot tail exceeds MAX_HOT_TAIL_BYTES",
        });
    }

    Ok(AssembledHotTail {
        hot_tail_raw_len,
        hot_tail_len,
        hot_tail_stored,
        is_lz4,
        lazy_data_buf,
        cur_file_off,
        schema_len,
        col_dir_off,
        rg_dir_off,
        block_dir_off,
        hot_bloom_off,
        hot_bloom_len,
        hot_dict_off,
        hot_dict_len,
        lazy_dir_off,
        lazy_dir_len,
    })
}

pub(super) fn decide_hot_tail_placement(
    base_compressed_len: usize,
    base_dirs_len: usize,
    trigram_count: usize,
    primary_bloom: &mut Option<(usize, Vec<u8>, bool)>,
    dict_items: &mut [(usize, Vec<u8>, bool)],
    sec_blooms: &mut [(usize, Vec<u8>, bool)],
) {
    let mut est_stored = POSTSCRIPT_LEN
        .saturating_add(base_compressed_len)
        .saturating_add(trigram_count.saturating_mul(LAZY_INDEX_ENTRY_LEN));
    let mut est_raw_len =
        base_dirs_len.saturating_add(trigram_count.saturating_mul(LAZY_INDEX_ENTRY_LEN));
    let remaining_after_primary = (dict_items.len() + sec_blooms.len()) * LAZY_INDEX_ENTRY_LEN;
    if let Some((_, bytes, in_hot)) = primary_bloom {
        if *in_hot
            && est_raw_len
                .saturating_add(bytes.len())
                .saturating_add(remaining_after_primary)
                .saturating_add(POSTSCRIPT_LEN)
                > MAX_HOT_TAIL_BYTES
        {
            *in_hot = false;
        }
        if *in_hot {
            est_stored = est_stored.saturating_add(bytes.len());
            est_raw_len = est_raw_len.saturating_add(bytes.len());
        } else {
            est_stored = est_stored.saturating_add(LAZY_INDEX_ENTRY_LEN);
            est_raw_len = est_raw_len.saturating_add(LAZY_INDEX_ENTRY_LEN);
        }
    }
    let mut remaining_lazy_entries = dict_items.len() + sec_blooms.len();
    for (_, bytes, in_hot) in dict_items.iter_mut() {
        remaining_lazy_entries = remaining_lazy_entries.saturating_sub(1);
        let reserve = remaining_lazy_entries * LAZY_INDEX_ENTRY_LEN;
        if bytes.len() <= HOT_DICT_MAX_BYTES
            && est_stored
                .saturating_add(bytes.len())
                .saturating_add(reserve)
                <= HOT_TAIL_SOFT_BUDGET_BYTES
            && est_raw_len
                .saturating_add(bytes.len())
                .saturating_add(reserve)
                .saturating_add(POSTSCRIPT_LEN)
                <= MAX_HOT_TAIL_BYTES
        {
            *in_hot = true;
            est_stored = est_stored.saturating_add(bytes.len());
            est_raw_len = est_raw_len.saturating_add(bytes.len());
        } else {
            *in_hot = false;
            est_stored = est_stored.saturating_add(LAZY_INDEX_ENTRY_LEN);
            est_raw_len = est_raw_len.saturating_add(LAZY_INDEX_ENTRY_LEN);
        }
    }
    for (_, bytes, in_hot) in sec_blooms.iter_mut() {
        remaining_lazy_entries = remaining_lazy_entries.saturating_sub(1);
        let reserve = remaining_lazy_entries * LAZY_INDEX_ENTRY_LEN;
        if est_stored
            .saturating_add(bytes.len())
            .saturating_add(reserve)
            <= HOT_TAIL_SOFT_BUDGET_BYTES
            && est_raw_len
                .saturating_add(bytes.len())
                .saturating_add(reserve)
                .saturating_add(POSTSCRIPT_LEN)
                <= MAX_HOT_TAIL_BYTES
        {
            *in_hot = true;
            est_stored = est_stored.saturating_add(bytes.len());
            est_raw_len = est_raw_len.saturating_add(bytes.len());
        } else {
            *in_hot = false;
            est_stored = est_stored.saturating_add(LAZY_INDEX_ENTRY_LEN);
            est_raw_len = est_raw_len.saturating_add(LAZY_INDEX_ENTRY_LEN);
        }
    }
}

pub(super) fn encode_narrow_trigram_section(
    filters: &[Option<Vec<u8>>],
) -> Result<Option<Vec<u8>>, StatamError> {
    if filters.iter().all(Option::is_none) {
        return Ok(None);
    }
    let count = u32::try_from(filters.len()).map_err(|_| StatamError::BoundsExceeded {
        reason: "narrow trigram block count overflows u32",
    })?;
    let mut body = Vec::new();
    let mut table = Vec::with_capacity(filters.len() * 8);
    for opt in filters {
        match opt {
            None => {
                table.extend_from_slice(&0u32.to_le_bytes());
                table.extend_from_slice(&0u32.to_le_bytes());
            }
            Some(bytes) => {
                let off = u32::try_from(body.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "narrow trigram body offset overflows u32",
                })?;
                let len = u32::try_from(bytes.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "narrow trigram filter len overflows u32",
                })?;
                table.extend_from_slice(&off.to_le_bytes());
                table.extend_from_slice(&len.to_le_bytes());
                body.extend_from_slice(bytes);
            }
        }
    }
    let total = 8 + table.len() + body.len();
    if total > MAX_BLOCK_RAW_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "narrow trigram section exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&NARROW_TRIGRAM_MAGIC);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&table);
    out.extend_from_slice(&body);
    Ok(Some(out))
}

fn append_lazy_entry(
    kind: LazyIndexKind,
    phys_col_id: u32,
    bytes: &[u8],
    cur_file_off: &mut u64,
    lazy_data_buf: &mut Vec<u8>,
    lazy_entries: &mut Vec<LazyIndexEntry>,
    col_dir: &mut ColumnDirEntry,
) -> Result<(), StatamError> {
    let len = u32::try_from(bytes.len()).map_err(|_| StatamError::BoundsExceeded {
        reason: "lazy index section overflows u32",
    })?;
    let off = *cur_file_off;
    let crc32c = crc32c::crc32c(bytes);
    lazy_data_buf.extend_from_slice(bytes);
    *cur_file_off =
        cur_file_off
            .checked_add(u64::from(len))
            .ok_or(StatamError::BoundsExceeded {
                reason: "lazy index file offset overflow",
            })?;
    lazy_entries.push(LazyIndexEntry {
        kind,
        reserved: [0; 3],
        phys_col_id,
        offset: off,
        len,
        crc32c,
    });
    match kind {
        LazyIndexKind::Bloom => {
            col_dir.bloom_offset = off;
            col_dir.bloom_len = len;
        }
        LazyIndexKind::Dict => {
            col_dir.dict_offset = off;
            col_dir.dict_len = len;
        }
        LazyIndexKind::NarrowTrigram => {
            col_dir.trigram_offset = off;
        }
    }
    Ok(())
}
