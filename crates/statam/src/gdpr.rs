//! Airtight single-rewrite GDPR erasure protocol (`spec-v0.md` §5.4).

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;

use cetra_column::{Batch, Bitmap, Column, DataType};

use crate::StatamError;
use crate::codec::WriteProfile;
use crate::format::{
    ChunkLoc, Codec, Encoding, FLAG_L1_COMPACT, LocalityGroup, MAX_BATCH_DECODED_BYTES,
    PACK_FLAG_HAS_DICT, PACK_FLAG_HAS_TRIGRAM, PACK_FLAG_TOC_LZ4, PACK_FLAG_TOC_ZSTD,
    PACK_FRAME_FLAG_USES_DICT,
};
use crate::io::RangeReader;
use crate::payload::pack::{ChunkPackBuilder, ChunkPackReader, PackWriteOptions, PackWriteStats};
use crate::payload::{PayloadRefRow, decode_payload_refs};
use crate::reader::StatamReader;
use crate::writer::{L0_LOCAL_PACK_IDX, StatamWriter, WriteOptions, WriteStats};

/// One live `.statam` file in a `(project_id, day)` partition passed to [`gdpr_rewrite_day`].
#[derive(Clone)]
pub struct GdprFileInput {
    /// Caller-assigned file identifier (opaque label, never logged with payload values).
    pub file_id: String,
    /// Range reader over the existing `.statam` file.
    pub reader: Arc<dyn RangeReader>,
    /// Optional [`WriteOptions`] override for rewriting; derived from file metadata if `None`.
    pub write_options: Option<WriteOptions>,
}

impl fmt::Debug for GdprFileInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GdprFileInput")
            .field("file_id", &self.file_id)
            .field("file_len", &self.reader.file_len())
            .finish()
    }
}

/// Newly rewritten `.statam` file produced by [`gdpr_rewrite_day`].
#[derive(Clone)]
pub struct RewrittenStatamFile {
    /// Original file identifier from [`GdprFileInput`].
    pub file_id: String,
    /// Complete serialized bytes of the rewritten `.statam` file.
    pub bytes: Vec<u8>,
    /// Writer statistics for the rewritten file.
    pub stats: WriteStats,
}

impl fmt::Debug for RewrittenStatamFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RewrittenStatamFile")
            .field("file_id", &self.file_id)
            .field("bytes_len", &self.bytes.len())
            .field("stats", &self.stats)
            .finish()
    }
}

/// Newly rewritten `.stpk` pack file produced by [`gdpr_rewrite_day`].
#[derive(Clone)]
pub struct RewrittenPackFile {
    /// Retired pack ID replaced by `new_pack_id`.
    pub old_pack_id: [u8; 16],
    /// Newly generated pack ID.
    pub new_pack_id: [u8; 16],
    /// Complete serialized bytes of the rewritten `.stpk` pack.
    pub bytes: Vec<u8>,
    /// Pack write statistics.
    pub stats: PackWriteStats,
}

impl fmt::Debug for RewrittenPackFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RewrittenPackFile")
            .field("bytes_len", &self.bytes.len())
            .field("stats", &self.stats)
            .finish()
    }
}

/// Summary counters emitted by [`gdpr_rewrite_day`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GdprRewriteStats {
    /// Number of `.statam` files rewritten.
    pub files_rewritten: usize,
    /// Number of `.stpk` packs rewritten (excluding 100%-dead packs dropped completely).
    pub packs_rewritten: usize,
    /// Number of 100%-live pack frames copied byte-for-byte without decompression.
    pub frames_copied_verbatim: usize,
    /// Number of mixed live/dead pack frames decompressed, purged, and recompressed.
    pub frames_rewritten: usize,
    /// Number of 0%-live pack frames dropped completely.
    pub frames_dropped: usize,
    /// Number of dead chunks purged across all packs.
    pub chunks_purged: usize,
    /// Total uncompressed bytes of purged chunks.
    pub bytes_purged: u64,
}

/// Output of [`gdpr_rewrite_day`].
#[derive(Debug, Clone)]
pub struct GdprRewriteResult {
    /// `.statam` files that had erased rows or referenced a rewritten/retired pack (rewritten once).
    pub rewritten_files: Vec<RewrittenStatamFile>,
    /// New `.stpk` packs replacing packs that had dead chunks.
    pub new_packs: Vec<RewrittenPackFile>,
    /// Old `.stpk` pack IDs to evict from caches and physically delete.
    pub retired_pack_ids: Vec<[u8; 16]>,
    /// Exact rewrite and purge counters.
    pub stats: GdprRewriteStats,
}

/// Alias matching `spec-v0.md` §2.
pub type GdprRewriteReport = GdprRewriteResult;

type PackRemapTable = HashMap<[u8; 16], ([u8; 16], Vec<Option<u32>>)>;

struct ScannedFilePlan {
    reader: StatamReader,
    has_erased: bool,
    rg_plans: Vec<RgSurvivingPlan>,
}

struct RgSurvivingPlan {
    non_payload_batch: Batch,
    surviving_indices: Vec<u32>,
    /// Surviving `PayloadRefRow`s per `ExternalPayload` logical column index.
    payload_refs: HashMap<usize, Vec<PayloadRefRow>>,
}

/// Executes the **single-rewrite GDPR erasure protocol** (`spec-v0.md` §5.4) across
/// all live `.statam` files and `.stpk` packs in a `(project_id, day)` partition:
///
/// 1. Evaluates `should_erase(&batch, row)` across non-payload columns and scans ONLY
///    `PayloadRefs` blocks on surviving rows across ALL live files to build exact
///    `live_chunks: Bitmap` per pack (zero `.stpk` payload decompression).
/// 2. Rewrites each pack with dead chunks (dropping 0%-live frames, copying 100%-live
///    frames verbatim byte-for-byte, and rewriting only mixed frames; unconditionally
///    discarding any old `DictSection`).
/// 3. Rewrites each affected or pack-referencing `.statam` file **once** with surviving
///    rows and remapped `(new_pack_idx, new_chunk_id)`.
pub fn gdpr_rewrite_day(
    files: &[GdprFileInput],
    packs: &HashMap<[u8; 16], Arc<dyn RangeReader>>,
    should_erase: &dyn Fn(&Batch, usize) -> bool,
) -> Result<GdprRewriteResult, StatamError> {
    // Open all packs and initialize live-chunk and boundary-chunk bitmaps.
    let mut pack_readers: HashMap<[u8; 16], Arc<ChunkPackReader>> =
        HashMap::with_capacity(packs.len());
    let mut live_chunks: HashMap<[u8; 16], Bitmap> = HashMap::with_capacity(packs.len());
    let mut boundary_utf8_chunks: HashMap<[u8; 16], Bitmap> = HashMap::with_capacity(packs.len());
    for (&pack_id, pack_reader) in packs {
        let opened = Arc::new(ChunkPackReader::open(Arc::clone(pack_reader))?);
        let chunk_count = opened.toc().chunks.len();
        pack_readers.insert(pack_id, opened);
        live_chunks.insert(pack_id, Bitmap::new_zeroed(chunk_count));
        boundary_utf8_chunks.insert(pack_id, Bitmap::new_zeroed(chunk_count));
    }

    // Step 1: Identify surviving rows across all files & mark live chunks per pack.
    let mut file_plans = Vec::with_capacity(files.len());
    for input in files {
        let statam = StatamReader::open(Arc::clone(&input.reader), None)?;
        let meta = statam.meta();
        for &pack_id in &meta.schema_meta.pack_ids {
            if !pack_readers.contains_key(&pack_id) {
                return Err(StatamError::MissingPack { pack_id });
            }
        }
        let num_rgs = meta.num_row_groups();
        let payload_cols: Vec<usize> = (0..meta.schema().len())
            .filter(|&i| meta.col_dir[i].locality_group == LocalityGroup::ExternalPayload)
            .collect();

        let mut has_erased = false;
        let mut rg_plans = Vec::with_capacity(num_rgs);

        for rg_idx in 0..num_rgs {
            let batch = statam.read_non_payload_rg_batch(rg_idx)?;
            let rg_rows = batch.raw_len();
            let mut surviving_indices = Vec::with_capacity(rg_rows);
            for r in 0..rg_rows {
                if should_erase(&batch, r) {
                    has_erased = true;
                } else {
                    surviving_indices.push(r as u32);
                }
            }

            let mut payload_refs: HashMap<usize, Vec<PayloadRefRow>> =
                HashMap::with_capacity(payload_cols.len());

            if !surviving_indices.is_empty() {
                for &cidx in &payload_cols {
                    let blks = meta.rg_blocks(cidx, rg_idx);
                    let mut all_rg_refs = Vec::with_capacity(rg_rows);
                    for blk in blks {
                        if blk.encoding == Encoding::ConstNull {
                            all_rg_refs.resize(
                                all_rg_refs.len() + blk.row_count as usize,
                                PayloadRefRow::Null,
                            );
                        } else {
                            let raw = statam.read_block_body(blk)?;
                            let decoded = decode_payload_refs(
                                &raw,
                                blk.row_count as usize,
                                MAX_BATCH_DECODED_BYTES,
                            )?;
                            all_rg_refs.extend(decoded);
                        }
                    }

                    let is_utf8_col = meta.schema().fields()[cidx].data_type == DataType::Utf8;
                    let mut surviving_refs = Vec::with_capacity(surviving_indices.len());
                    for &idx in &surviving_indices {
                        let row_ref = &all_rg_refs[idx as usize];
                        if let PayloadRefRow::Chunks(refs) = row_ref {
                            let is_multi_chunk_utf8 = is_utf8_col && refs.len() > 1;
                            for &(pidx, cid) in refs {
                                if pidx != L0_LOCAL_PACK_IDX {
                                    let pack_id = *meta
                                        .schema_meta
                                        .pack_ids
                                        .get(pidx as usize)
                                        .ok_or(StatamError::Corrupt {
                                            reason: "payload ref pack_idx out of bounds during GDPR",
                                        })?;
                                    let bm = live_chunks
                                        .get_mut(&pack_id)
                                        .ok_or(StatamError::MissingPack { pack_id })?;
                                    if (cid as usize) >= bm.len() {
                                        return Err(StatamError::Corrupt {
                                            reason: "payload ref chunk_id out of pack bounds during GDPR",
                                        });
                                    }
                                    let _ = bm.set(cid as usize, true);
                                    if is_multi_chunk_utf8
                                        && let Some(b_bm) = boundary_utf8_chunks.get_mut(&pack_id)
                                    {
                                        let _ = b_bm.set(cid as usize, true);
                                    }
                                }
                            }
                        }
                        surviving_refs.push(row_ref.clone());
                    }
                    payload_refs.insert(cidx, surviving_refs);
                }
            }

            rg_plans.push(RgSurvivingPlan {
                non_payload_batch: batch,
                surviving_indices,
                payload_refs,
            });
        }

        file_plans.push(ScannedFilePlan {
            reader: statam,
            has_erased,
            rg_plans,
        });
    }

    // Step 2: Rewrite any pack that has dead chunks (or a legacy DictSection).
    let mut stats = GdprRewriteStats::default();
    let mut new_packs = Vec::new();
    let mut retired_pack_ids = Vec::new();
    let mut pack_remap: PackRemapTable = HashMap::new();
    let mut fully_dead_packs: BTreeSet<[u8; 16]> = BTreeSet::new();

    let mut sorted_pack_ids: Vec<[u8; 16]> = pack_readers.keys().copied().collect();
    sorted_pack_ids.sort_unstable();

    for old_pack_id in sorted_pack_ids {
        let pack = &pack_readers[&old_pack_id];
        let live = &live_chunks[&old_pack_id];
        let total_chunks = live.len();
        let live_count = live.count_ones();
        let has_legacy_dict = (pack.postscript().flags & PACK_FLAG_HAS_DICT) != 0;

        if live_count == total_chunks && !has_legacy_dict {
            continue;
        }

        let toc = pack.toc();
        if live_count == 0 {
            stats.frames_dropped += toc.frames.len();
            stats.chunks_purged += total_chunks;
            for loc in &toc.chunks {
                stats.bytes_purged = stats.bytes_purged.saturating_add(u64::from(loc.raw_len));
            }
            retired_pack_ids.push(old_pack_id);
            fully_dead_packs.insert(old_pack_id);
            continue;
        }

        stats.packs_rewritten += 1;
        let new_pack_id = derive_gdpr_pack_id(old_pack_id);
        let pack_opts = PackWriteOptions {
            codec: default_gdpr_pack_codec(),
            zstd_level: 3,
            target_frame_raw_bytes: 64 * 1024,
            enable_trigram_skip: (toc.postscript.flags & PACK_FLAG_HAS_TRIGRAM) != 0
                || toc.frames.iter().any(|f| f.filter_len > 0),
            compress_toc_lz4: (toc.postscript.flags & (PACK_FLAG_TOC_LZ4 | PACK_FLAG_TOC_ZSTD))
                != 0,
            emit_hash_table: toc.postscript.hash_table_off != 0,
            write_threads: 1,
        };
        let mut builder = ChunkPackBuilder::new(new_pack_id, pack_opts);
        builder.enable_boundary_tracking();
        let boundary_utf8 = &boundary_utf8_chunks[&old_pack_id];

        let hashes = pack.read_hash_table()?;
        let has_hashes = hashes.len() == total_chunks;
        let trigrams = pack.read_trigram_section()?;
        let edges = pack.read_edge_table()?;
        let mut chunk_remap = vec![None; total_chunks];

        if toc.frames_contiguous {
            for (fid, f_entry) in toc.frames.iter().enumerate() {
                let first_cid = f_entry.first_chunk_id as usize;
                let cnt = f_entry.chunk_count as usize;
                let live_in_frame = (first_cid..first_cid + cnt)
                    .filter(|&c| live.get(c))
                    .count();

                if live_in_frame == 0 {
                    stats.frames_dropped += 1;
                    stats.chunks_purged += cnt;
                    for cid in first_cid..first_cid + cnt {
                        stats.bytes_purged = stats
                            .bytes_purged
                            .saturating_add(u64::from(toc.chunks[cid].raw_len));
                    }
                    continue;
                }

                let has_utf8_boundary_in_frame =
                    (first_cid..first_cid + cnt).any(|cid| boundary_utf8.get(cid));
                let edges_ok = if edges.is_empty() {
                    !has_utf8_boundary_in_frame
                } else {
                    edges.len() == total_chunks
                        && (first_cid..first_cid + cnt).all(|cid| {
                            let loc = toc.chunks[cid];
                            !(boundary_utf8.get(cid)
                                && loc.raw_len > 0
                                && (edges[cid].0.is_empty() || edges[cid].1.is_empty()))
                        })
                };

                if live_in_frame == cnt
                    && frame_has_no_slack(&toc.chunks[first_cid..first_cid + cnt], f_entry.raw_len)
                    && (f_entry.flags & PACK_FRAME_FLAG_USES_DICT) == 0
                    && has_hashes
                    && edges_ok
                {
                    let raw_frame = pack.read_frame_raw(fid as u32)?;
                    let chunk_meta: Vec<([u8; 16], u32, u32)> = (first_cid..first_cid + cnt)
                        .map(|cid| {
                            let loc = toc.chunks[cid];
                            (hashes[cid], loc.offset_in_frame, loc.raw_len)
                        })
                        .collect();
                    let filter_slice = trigrams[fid].as_ref().map(|b| b.as_bytes()).unwrap_or(&[]);
                    let empty_edges: Vec<(Vec<u8>, Vec<u8>)>;
                    let edge_slice = if edges.is_empty() {
                        empty_edges = vec![(Vec::new(), Vec::new()); cnt];
                        &empty_edges[..]
                    } else {
                        &edges[first_cid..first_cid + cnt]
                    };

                    let new_cids = builder.append_raw_frame(
                        &raw_frame,
                        *f_entry,
                        &chunk_meta,
                        filter_slice,
                        edge_slice,
                    )?;
                    for (offset, new_cid) in new_cids.into_iter().enumerate() {
                        let old_cid = first_cid + offset;
                        chunk_remap[old_cid] = Some(new_cid);
                        if boundary_utf8.get(old_cid) {
                            builder.mark_boundary_chunk(new_cid);
                        }
                    }
                    stats.frames_copied_verbatim += 1;
                } else {
                    let decoded = pack.read_frame_decompressed(fid as u32)?;
                    for (old_cid, slot) in
                        chunk_remap.iter_mut().enumerate().skip(first_cid).take(cnt)
                    {
                        let loc = toc.chunks[old_cid];
                        if live.get(old_cid) {
                            let start = loc.offset_in_frame as usize;
                            let end = start + loc.raw_len as usize;
                            let slice = decoded.get(start..end).ok_or(StatamError::Corrupt {
                                reason: "chunk slice out of decompressed frame bounds during GDPR",
                            })?;
                            let new_cid = builder.intern_chunk(slice)?;
                            *slot = Some(new_cid);
                            if boundary_utf8.get(old_cid) {
                                builder.mark_boundary_chunk(new_cid);
                            }
                        } else {
                            stats.chunks_purged += 1;
                            stats.bytes_purged =
                                stats.bytes_purged.saturating_add(u64::from(loc.raw_len));
                        }
                    }
                    stats.frames_rewritten += 1;
                }
            }
        } else {
            let mut chunks_by_frame: Vec<Vec<usize>> = vec![Vec::new(); toc.frames.len()];
            for (old_cid, loc) in toc.chunks.iter().enumerate() {
                chunks_by_frame[loc.frame_id as usize].push(old_cid);
            }
            for (fid, frame_cids) in chunks_by_frame.into_iter().enumerate() {
                let mut live_cids = Vec::new();
                for old_cid in frame_cids {
                    if live.get(old_cid) {
                        live_cids.push(old_cid);
                    } else {
                        stats.chunks_purged += 1;
                        stats.bytes_purged = stats
                            .bytes_purged
                            .saturating_add(u64::from(toc.chunks[old_cid].raw_len));
                    }
                }
                if live_cids.is_empty() {
                    stats.frames_dropped += 1;
                    continue;
                }
                let decoded = pack.read_frame_decompressed(fid as u32)?;
                for old_cid in live_cids {
                    let loc = toc.chunks[old_cid];
                    let start = loc.offset_in_frame as usize;
                    let end = start + loc.raw_len as usize;
                    let slice = decoded.get(start..end).ok_or(StatamError::Corrupt {
                        reason: "chunk slice out of decompressed frame bounds during GDPR",
                    })?;
                    let new_cid = builder.intern_chunk(slice)?;
                    chunk_remap[old_cid] = Some(new_cid);
                    if boundary_utf8.get(old_cid) {
                        builder.mark_boundary_chunk(new_cid);
                    }
                }
                stats.frames_rewritten += 1;
            }
        }

        let mut new_bytes = Vec::new();
        let pack_stats = builder.finish(&mut new_bytes)?;
        new_packs.push(RewrittenPackFile {
            old_pack_id,
            new_pack_id,
            bytes: new_bytes,
            stats: pack_stats,
        });
        retired_pack_ids.push(old_pack_id);
        pack_remap.insert(old_pack_id, (new_pack_id, chunk_remap));
    }

    // Step 3: Rewrite each affected or pack-referencing .statam file ONCE.
    let mut rewritten_files = Vec::new();
    for (input, plan) in files.iter().zip(file_plans) {
        let meta = plan.reader.meta();
        let touches_rewritten_pack = meta
            .schema_meta
            .pack_ids
            .iter()
            .any(|pid| pack_remap.contains_key(pid) || fully_dead_packs.contains(pid));

        if !plan.has_erased && !touches_rewritten_pack {
            continue;
        }

        stats.files_rewritten += 1;
        let expected_payload_cols: Vec<String> = (0..meta.schema().len())
            .filter(|&i| meta.col_dir[i].locality_group == LocalityGroup::ExternalPayload)
            .map(|i| meta.schema().fields()[i].name.clone())
            .collect();
        let write_opts = match &input.write_options {
            Some(opts) => {
                let mut cloned = opts.clone();
                cloned.payload_cols = expected_payload_cols;
                cloned
            }
            None => derive_write_options_from_meta(meta),
        };

        // Build remapped pack_ids list strictly from packs actually referenced by surviving rows.
        let mut used_old_pidxs = BTreeSet::new();
        for rg_plan in &plan.rg_plans {
            if rg_plan.surviving_indices.is_empty() {
                continue;
            }
            for rows in rg_plan.payload_refs.values() {
                for row in rows {
                    if let PayloadRefRow::Chunks(refs) = row {
                        for &(pidx, _) in refs {
                            if pidx != L0_LOCAL_PACK_IDX {
                                used_old_pidxs.insert(pidx);
                            }
                        }
                    }
                }
            }
        }

        let mut new_pack_ids: Vec<[u8; 16]> = Vec::new();
        let mut pack_id_to_new_idx: HashMap<[u8; 16], u16> = HashMap::new();
        for old_pidx in used_old_pidxs {
            let old_pid =
                *meta
                    .schema_meta
                    .pack_ids
                    .get(old_pidx as usize)
                    .ok_or(StatamError::Corrupt {
                        reason: "surviving payload ref pack_idx out of bounds during GDPR",
                    })?;
            if fully_dead_packs.contains(&old_pid) {
                return Err(StatamError::Corrupt {
                    reason: "surviving row references retired 100%-dead pack",
                });
            }
            let target_pid = pack_remap
                .get(&old_pid)
                .map_or(old_pid, |(new_pid, _)| *new_pid);
            if let std::collections::hash_map::Entry::Vacant(e) =
                pack_id_to_new_idx.entry(target_pid)
            {
                let idx =
                    u16::try_from(new_pack_ids.len()).map_err(|_| StatamError::BoundsExceeded {
                        reason: "rewritten pack_ids exceeds u16::MAX",
                    })?;
                new_pack_ids.push(target_pid);
                e.insert(idx);
            }
        }

        let mut new_bytes = Vec::new();
        let mut writer = StatamWriter::new(
            std::io::Cursor::new(&mut new_bytes),
            Arc::clone(meta.schema()),
            write_opts,
            None,
        )?;
        writer.set_pack_ids(new_pack_ids);

        for (rg_idx, rg_plan) in plan.rg_plans.into_iter().enumerate() {
            if rg_plan.surviving_indices.is_empty() {
                continue;
            }

            let surviving_batch = gather_surviving_non_payload_batch(
                &rg_plan.non_payload_batch,
                &rg_plan.surviving_indices,
            )?;

            let mut has_l0_chunks = false;
            let mut remapped_refs: HashMap<usize, Vec<PayloadRefRow>> =
                HashMap::with_capacity(rg_plan.payload_refs.len());

            for (cidx, rows) in rg_plan.payload_refs {
                let mut out_rows = Vec::with_capacity(rows.len());
                for row in rows {
                    match row {
                        PayloadRefRow::Null => out_rows.push(PayloadRefRow::Null),
                        PayloadRefRow::Inline(b) => out_rows.push(PayloadRefRow::Inline(b)),
                        PayloadRefRow::Chunks(refs) => {
                            let mut new_refs = Vec::with_capacity(refs.len());
                            for (old_pidx, old_cid) in refs {
                                if old_pidx == L0_LOCAL_PACK_IDX {
                                    has_l0_chunks = true;
                                    new_refs.push((L0_LOCAL_PACK_IDX, old_cid));
                                } else {
                                    let old_pid = *meta
                                        .schema_meta
                                        .pack_ids
                                        .get(old_pidx as usize)
                                        .ok_or(StatamError::Corrupt {
                                            reason: "payload ref pack_idx out of bounds during GDPR remap",
                                        })?;
                                    if let Some((new_pid, remap)) = pack_remap.get(&old_pid) {
                                        let new_cid = remap
                                            .get(old_cid as usize)
                                            .and_then(|opt| *opt)
                                            .ok_or(StatamError::Corrupt {
                                                reason: "surviving chunk missing from GDPR remap",
                                            })?;
                                        let new_pidx = pack_id_to_new_idx[new_pid];
                                        new_refs.push((new_pidx, new_cid));
                                    } else {
                                        let new_pidx = pack_id_to_new_idx[&old_pid];
                                        new_refs.push((new_pidx, old_cid));
                                    }
                                }
                            }
                            out_rows.push(PayloadRefRow::Chunks(new_refs));
                        }
                    }
                }
                remapped_refs.insert(cidx, out_rows);
            }

            let mut l0_source = if has_l0_chunks {
                let src =
                    plan.reader
                        .open_l0_payload_for_rg(rg_idx)?
                        .ok_or(StatamError::Corrupt {
                            reason: "missing L0 payload region for L0 chunk reference",
                        })?;
                Some(src)
            } else {
                None
            };

            writer.write_batch_with_prechunked_refs(
                &surviving_batch,
                &remapped_refs,
                l0_source.as_mut(),
            )?;
        }

        let write_stats = writer.finish()?;
        rewritten_files.push(RewrittenStatamFile {
            file_id: input.file_id.clone(),
            bytes: new_bytes,
            stats: write_stats,
        });
    }

    Ok(GdprRewriteResult {
        rewritten_files,
        new_packs,
        retired_pack_ids,
        stats,
    })
}

/// Returns `true` iff `chunks` cover `0..raw_len` completely with zero unreferenced slack bytes.
fn frame_has_no_slack(chunks: &[ChunkLoc], raw_len: u32) -> bool {
    let mut cursor = 0u32;
    let mut fast_ok = true;
    for loc in chunks {
        if loc.offset_in_frame != cursor {
            fast_ok = false;
            break;
        }
        let Some(next) = cursor.checked_add(loc.raw_len) else {
            return false;
        };
        cursor = next;
    }
    if fast_ok {
        return cursor == raw_len;
    }

    let mut intervals: Vec<(u32, u32)> = Vec::with_capacity(chunks.len());
    for loc in chunks {
        let Some(end) = loc.offset_in_frame.checked_add(loc.raw_len) else {
            return false;
        };
        if end > raw_len {
            return false;
        }
        if loc.raw_len > 0 {
            intervals.push((loc.offset_in_frame, end));
        }
    }
    intervals.sort_unstable_by_key(|&(start, _)| start);
    let mut covered_end = 0u32;
    for (start, end) in intervals {
        if start > covered_end {
            return false;
        }
        if end > covered_end {
            covered_end = end;
        }
    }
    covered_end == raw_len
}

fn gather_surviving_non_payload_batch(
    batch: &Batch,
    surviving_indices: &[u32],
) -> Result<Batch, StatamError> {
    if surviving_indices.len() == batch.raw_len() {
        return Ok(batch.clone());
    }
    let schema = batch.schema();
    let new_len = surviving_indices.len();
    let mut new_cols = Vec::with_capacity(schema.len());
    for col in batch.columns() {
        if let Column::Const { value, .. } = col {
            new_cols.push(Column::try_new_const(value.clone(), new_len)?);
        } else {
            let flat = col.take(surviving_indices, None)?;
            new_cols.push(Column::Flat(flat));
        }
    }
    Ok(Batch::try_new_all(Arc::clone(schema), new_cols, new_len)?)
}

fn derive_write_options_from_meta(meta: &crate::reader::StatamFileMeta) -> WriteOptions {
    let profile = if (meta.postscript.flags & FLAG_L1_COMPACT) != 0 {
        WriteProfile::L1Compact
    } else {
        WriteProfile::L0Fast
    };
    let fields = meta.schema().fields();
    let mut opts = WriteOptions::for_table(meta.schema_meta.table_kind, profile);
    opts.sort_cols = meta.schema_meta.sort_cols.clone();
    opts.payload_cols = fields
        .iter()
        .enumerate()
        .filter(|&(i, f)| {
            matches!(f.data_type, DataType::Utf8 | DataType::Binary)
                && meta.col_dir[i].locality_group == LocalityGroup::ExternalPayload
        })
        .map(|(_, f)| f.name.clone())
        .collect();

    let mut bloom_cols = Vec::new();
    let mut hot_bloom: Option<String> = None;
    let mut trigram_cols = Vec::new();
    for (i, f) in fields.iter().enumerate() {
        let cd = &meta.col_dir[i];
        if cd.bloom_len > 0 {
            bloom_cols.push(f.name.clone());
            if (cd.col_flags & crate::format::COL_FLAG_BLOOM_IN_HOT_TAIL) != 0
                && (f.name == "trace_id" || hot_bloom.is_none())
            {
                hot_bloom = Some(f.name.clone());
            }
        }
        if (cd.col_flags & crate::format::COL_FLAG_HAS_TRIGRAM_INDEX) != 0 {
            trigram_cols.push(f.name.clone());
        }
    }
    if hot_bloom.is_none() && bloom_cols.iter().any(|n| n == "trace_id") {
        hot_bloom = Some("trace_id".to_owned());
    }
    opts.bloom_cols = bloom_cols;
    opts.hot_bloom_col = hot_bloom;
    opts.narrow_trigram_cols = trigram_cols;
    opts
}

fn derive_gdpr_pack_id(old_pack_id: [u8; 16]) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"statam-gdpr-rewrite-v1:");
    hasher.update(&old_pack_id);
    let hash = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&hash.as_bytes()[..16]);
    if out == old_pack_id {
        out[0] ^= 0x01;
    }
    out
}

#[cfg(not(miri))]
fn default_gdpr_pack_codec() -> Codec {
    Codec::Zstd
}

#[cfg(miri)]
fn default_gdpr_pack_codec() -> Codec {
    Codec::Lz4
}

#[cfg(test)]
mod tests {
    use cetra_column::{Field, FlatColumn, FlatData, Schema, Utf8Data};
    use memchr::memmem;

    use super::*;
    use crate::format::{MAX_BLOCK_RAW_BYTES, TableKind, read_framed_block};
    use crate::io::SliceReader;
    use crate::reader::{InMemoryPackResolver, PackResolver, ScanRequest};

    fn test_pack_opts() -> PackWriteOptions {
        PackWriteOptions {
            codec: default_gdpr_pack_codec(),
            zstd_level: 3,
            target_frame_raw_bytes: 16 * 1024,
            enable_trigram_skip: true,
            compress_toc_lz4: true,
            emit_hash_table: true,
            write_threads: 1,
        }
    }

    fn make_gdpr_schema() -> Arc<Schema> {
        Arc::new(
            Schema::try_new(vec![
                Field::new("user_id", DataType::Utf8, false),
                Field::new("name", DataType::Utf8, false),
                Field::new("input", DataType::Utf8, true),
            ])
            .unwrap(),
        )
    }

    fn build_gdpr_batch(schema: &Arc<Schema>, rows: &[(&str, &str, &str)]) -> Batch {
        let uids: Vec<&str> = rows.iter().map(|r| r.0).collect();
        let names: Vec<&str> = rows.iter().map(|r| r.1).collect();
        let inputs: Vec<&str> = rows.iter().map(|r| r.2).collect();
        Batch::try_new_all(
            Arc::clone(schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(uids).unwrap()),
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
                        FlatData::Utf8(Utf8Data::try_from_slices(inputs).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            rows.len(),
        )
        .unwrap()
    }

    #[test]
    fn gdpr_rewrite_day_leaves_zero_erased_bytes_raw_and_decompressed_and_copies_live_frames() {
        let schema = Schema::try_new_arc(vec![
            Field::new("user_id", DataType::Utf8, false),
            Field::new("name", DataType::Utf8, false),
            Field::new("input", DataType::Utf8, true),
        ])
        .unwrap();

        // Build a shared .stpk pack with 3 distinct frame regimes (`target_frame_raw_bytes = 16 KiB`,
        // each chunk <= 8192 B so structural/CDC keeps it as 1 chunk):
        // - Frame 0: 100% dead (2 chunks belonging only to erased user `user_erased` -> dropped!)
        // - Frame 1: Mixed (1 erased + 1 surviving -> decompressed & purged!)
        // - Frame 2: 100% live (2 chunks belonging only to surviving users -> copied byte-for-byte verbatim!)
        let pack_id = [0x91u8; 16];
        let mut pb = ChunkPackBuilder::new(pack_id, test_pack_opts());

        let erased_secret_0a = format!("ERASED_PII_MARKER_ALPHA_{}", "x".repeat(8_000));
        let erased_secret_0b = format!("ERASED_PII_MARKER_GAMMA_{}", "w".repeat(8_168));
        let erased_secret_1 = format!("ERASED_PII_MARKER_BETA_{}", "y".repeat(8_000));
        let shared_keep = format!("SHARED_SURVIVING_SYSTEM_PROMPT_{}", "s".repeat(8_160));
        let live_frame2_a = format!("LIVE_VERBATIM_FRAME_A_{}", "z".repeat(8_000));
        let live_frame2_b = format!("LIVE_VERBATIM_FRAME_B_{}", "q".repeat(8_160));
        let inline_erased = "ERASED_INLINE_SECRET_999";

        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        opts.payload_cols = vec!["input".to_owned()];
        opts.pack_id = Some(pack_id);

        // File A (has erased rows + surviving rows)
        let batch_a = build_gdpr_batch(
            &schema,
            &[
                ("user_erased", "span_0", &erased_secret_0a),
                ("user_erased", "span_0b", &erased_secret_0b),
                ("user_erased", "span_1", &erased_secret_1),
                ("user_keep_1", "span_2", &shared_keep),
                ("user_erased", "span_3", inline_erased),
            ],
        );
        let mut file_a_bytes = Vec::new();
        {
            let mut wa = StatamWriter::new(
                std::io::Cursor::new(&mut file_a_bytes),
                Arc::clone(&schema),
                opts.clone(),
                Some(&mut pb),
            )
            .unwrap();
            wa.write_batch(&batch_a).unwrap();
            wa.finish().unwrap();
        }

        // File B (100% surviving rows, referencing pack Frame 2 -> untouched rows, BUT references rewritten pack!)
        let batch_b = build_gdpr_batch(
            &schema,
            &[
                ("user_keep_2", "span_4", &live_frame2_a),
                ("user_keep_2", "span_4b", &live_frame2_b),
            ],
        );
        let mut file_b_bytes = Vec::new();
        {
            let mut wb = StatamWriter::new(
                std::io::Cursor::new(&mut file_b_bytes),
                Arc::clone(&schema),
                opts,
                Some(&mut pb),
            )
            .unwrap();
            wb.write_batch(&batch_b).unwrap();
            wb.finish().unwrap();
        }

        // File C (self-contained L0Fast with no erased rows -> must NOT be rewritten!)
        let mut opts_c = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
        opts_c.payload_cols = vec!["input".to_owned()];
        let batch_c = build_gdpr_batch(&schema, &[("user_keep_3", "span_5", "clean_l0_payload")]);
        let mut file_c_bytes = Vec::new();
        {
            let mut wc = StatamWriter::new(
                std::io::Cursor::new(&mut file_c_bytes),
                Arc::clone(&schema),
                opts_c,
                None,
            )
            .unwrap();
            wc.write_batch(&batch_c).unwrap();
            wc.finish().unwrap();
        }

        let mut old_pack_bytes = Vec::new();
        pb.finish(&mut old_pack_bytes).unwrap();
        let old_pack_reader =
            ChunkPackReader::open(Arc::new(SliceReader::new(old_pack_bytes.clone()))).unwrap();
        assert_eq!(old_pack_reader.toc().frames.len(), 3);

        let inputs = vec![
            GdprFileInput {
                file_id: "file_a".to_owned(),
                reader: Arc::new(SliceReader::new(file_a_bytes)),
                write_options: None,
            },
            GdprFileInput {
                file_id: "file_b".to_owned(),
                reader: Arc::new(SliceReader::new(file_b_bytes)),
                write_options: None,
            },
            GdprFileInput {
                file_id: "file_c".to_owned(),
                reader: Arc::new(SliceReader::new(file_c_bytes)),
                write_options: None,
            },
        ];
        let mut packs: HashMap<[u8; 16], Arc<dyn RangeReader>> = HashMap::new();
        packs.insert(pack_id, Arc::new(SliceReader::new(old_pack_bytes)));

        let report = gdpr_rewrite_day(&inputs, &packs, &|batch, row| {
            matches!(
                batch.column_by_name("user_id").and_then(|c| c.get_scalar(row).ok()),
                Some(cetra_column::ScalarValue::Utf8(ref u)) if u == "user_erased"
            )
        })
        .unwrap();

        assert_eq!(
            report.stats.files_rewritten, 2,
            "only file_a and file_b should be rewritten"
        );
        assert_eq!(report.stats.packs_rewritten, 1);
        assert_eq!(report.stats.frames_dropped, 1, "Frame 0 must be dropped");
        assert_eq!(
            report.stats.frames_rewritten, 1,
            "Frame 1 must be rewritten"
        );
        assert_eq!(
            report.stats.frames_copied_verbatim, 1,
            "Frame 2 must be copied verbatim"
        );
        assert_eq!(report.stats.chunks_purged, 3);
        assert_eq!(report.retired_pack_ids, vec![pack_id]);
        assert_eq!(report.new_packs.len(), 1);

        let new_pack = &report.new_packs[0];
        let new_pack_reader =
            ChunkPackReader::open(Arc::new(SliceReader::new(new_pack.bytes.clone()))).unwrap();
        assert_ne!(
            old_pack_reader.postscript().flags & (PACK_FLAG_TOC_LZ4 | PACK_FLAG_TOC_ZSTD),
            0,
            "source pack must have compressed TOC"
        );
        assert_ne!(
            new_pack_reader.postscript().flags & (PACK_FLAG_TOC_LZ4 | PACK_FLAG_TOC_ZSTD),
            0,
            "rewritten pack must retain compressed TOC flag"
        );

        // Proof 1: Verbatim raw frame copy check for Frame 2!
        let old_f2_raw = old_pack_reader.read_frame_raw(2).unwrap();
        let copied_raw = new_pack_reader.read_frame_raw(1).unwrap();
        assert_eq!(
            copied_raw, old_f2_raw,
            "100%-live frame must be copied byte-for-byte"
        );

        // Proof 2: ZERO erased bytes in raw AND decompressed bytes of ALL rewritten packs & files!
        let erased_needles: [&[u8]; 5] = [
            b"user_erased",
            b"ERASED_PII_MARKER_ALPHA",
            b"ERASED_PII_MARKER_BETA",
            b"ERASED_PII_MARKER_GAMMA",
            b"ERASED_INLINE_SECRET_999",
        ];

        // Check raw & decompressed pack frames
        for needle in erased_needles {
            assert!(
                memmem::find(&new_pack.bytes, needle).is_none(),
                "raw new_pack contained erased marker!"
            );
        }
        for fid in 0..new_pack_reader.toc().frames.len() as u32 {
            let dec = new_pack_reader.read_frame_decompressed(fid).unwrap();
            for needle in erased_needles {
                assert!(
                    memmem::find(&dec, needle).is_none(),
                    "decompressed pack frame {fid} contained erased marker!"
                );
            }
        }

        // Check raw & decompressed .statam blocks + readable surviving data
        let mut resolver = InMemoryPackResolver::new();
        resolver.insert(new_pack.new_pack_id, Arc::new(new_pack_reader));
        let resolver: Arc<dyn PackResolver> = Arc::new(resolver);

        for rw in &report.rewritten_files {
            for needle in erased_needles {
                assert!(
                    memmem::find(&rw.bytes, needle).is_none(),
                    "raw rewritten file {} contained erased marker!",
                    rw.file_id
                );
            }
            let r = StatamReader::open(
                Arc::new(SliceReader::new(rw.bytes.clone())),
                Some(Arc::clone(&resolver)),
            )
            .unwrap();
            r.verify_full().unwrap();

            for blk in &r.meta().block_dir {
                if blk.stored_len > 0 {
                    let s = blk.offset as usize;
                    let e = s + crate::format::BLOCK_FRAME_HEADER_LEN + blk.stored_len as usize;
                    let (_, _, body) =
                        read_framed_block(&rw.bytes[s..e], MAX_BLOCK_RAW_BYTES).unwrap();
                    for needle in erased_needles {
                        assert!(
                            memmem::find(&body, needle).is_none(),
                            "decompressed block in {} contained erased marker!",
                            rw.file_id
                        );
                    }
                }
            }

            let batches = r.scan(&ScanRequest::default()).unwrap();
            assert_eq!(batches.len(), 1);
            if rw.file_id == "file_a" {
                assert_eq!(batches[0].num_rows(), 1);
                assert_eq!(
                    batches[0].column(2).unwrap().get_scalar(0).unwrap(),
                    cetra_column::ScalarValue::Utf8(shared_keep.clone())
                );
            } else if rw.file_id == "file_b" {
                assert_eq!(batches[0].num_rows(), 2);
                assert_eq!(
                    batches[0].column(2).unwrap().get_scalar(0).unwrap(),
                    cetra_column::ScalarValue::Utf8(live_frame2_a.clone())
                );
                assert_eq!(
                    batches[0].column(2).unwrap().get_scalar(1).unwrap(),
                    cetra_column::ScalarValue::Utf8(live_frame2_b.clone())
                );
            }
        }
    }

    #[test]
    fn gdpr_rewrite_purges_l0_local_payloads_and_handles_fully_erased_file() {
        let schema = Schema::try_new_arc(vec![
            Field::new("user_id", DataType::Utf8, false),
            Field::new("input", DataType::Utf8, true),
        ])
        .unwrap();

        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
        opts.payload_cols = vec!["input".to_owned()];

        let dead_l0 = format!("DEAD_L0_SECRET_{}", "d".repeat(600));
        let live_l0 = format!("LIVE_L0_KEEP_{}", "k".repeat(600));
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(["u_dead", "u_live"]).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices([dead_l0.as_str(), live_l0.as_str()])
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

        let mut l0_bytes = Vec::new();
        let mut w = StatamWriter::new(
            std::io::Cursor::new(&mut l0_bytes),
            Arc::clone(&schema),
            opts,
            None,
        )
        .unwrap();
        w.write_batch(&batch).unwrap();
        w.finish().unwrap();

        let inputs = vec![GdprFileInput {
            file_id: "l0_file".to_owned(),
            reader: Arc::new(SliceReader::new(l0_bytes)),
            write_options: None,
        }];
        let packs = HashMap::new();
        let report = gdpr_rewrite_day(&inputs, &packs, &|b, row| {
            matches!(
                b.column_by_name("user_id").and_then(|c| c.get_scalar(row).ok()),
                Some(cetra_column::ScalarValue::Utf8(ref u)) if u == "u_dead"
            )
        })
        .unwrap();

        assert_eq!(report.rewritten_files.len(), 1);
        let rw = &report.rewritten_files[0];
        assert!(memmem::find(&rw.bytes, b"DEAD_L0_SECRET").is_none());
        assert!(memmem::find(&rw.bytes, b"u_dead").is_none());

        let r = StatamReader::open(Arc::new(SliceReader::new(rw.bytes.clone())), None).unwrap();
        r.verify_full().unwrap();
        let out = r.scan(&ScanRequest::default()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].num_rows(), 1);
        assert_eq!(
            out[0].column(1).unwrap().get_scalar(0).unwrap(),
            cetra_column::ScalarValue::Utf8(format!("LIVE_L0_KEEP_{}", "k".repeat(600)))
        );
    }

    #[test]
    fn gdpr_rewrite_handles_100_percent_erased_l1_and_l0_files() {
        let schema = Schema::try_new_arc(vec![
            Field::new("user_id", DataType::Utf8, false),
            Field::new("name", DataType::Utf8, false),
            Field::new("input", DataType::Utf8, true),
        ])
        .unwrap();

        let pack_id = [0x44u8; 16];
        let mut pb = ChunkPackBuilder::new(pack_id, test_pack_opts());
        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        opts.payload_cols = vec!["input".to_owned()];
        opts.pack_id = Some(pack_id);

        let secret = format!("ALL_ERASED_SECRET_{}", "z".repeat(1024));
        let batch = build_gdpr_batch(&schema, &[("u_all_dead", "op", &secret)]);
        let mut file_bytes = Vec::new();
        {
            let mut w = StatamWriter::new(
                std::io::Cursor::new(&mut file_bytes),
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

        let inputs = vec![GdprFileInput {
            file_id: "dead_l1".to_owned(),
            reader: Arc::new(SliceReader::new(file_bytes)),
            write_options: None,
        }];
        let mut packs: HashMap<[u8; 16], Arc<dyn RangeReader>> = HashMap::new();
        packs.insert(pack_id, Arc::new(SliceReader::new(pack_bytes)));

        // Erase 100% of rows!
        let report = gdpr_rewrite_day(&inputs, &packs, &|_, _| true).unwrap();
        assert_eq!(report.stats.files_rewritten, 1);
        assert_eq!(report.stats.packs_rewritten, 0);
        assert_eq!(report.retired_pack_ids, vec![pack_id]);
        assert!(report.new_packs.is_empty());

        let rw = &report.rewritten_files[0];
        assert_eq!(rw.stats.row_count, 0);
        assert!(rw.stats.pack_ids.is_empty());
        assert!(memmem::find(&rw.bytes, b"ALL_ERASED_SECRET").is_none());
        assert!(memmem::find(&rw.bytes, b"u_all_dead").is_none());

        let r = StatamReader::open(Arc::new(SliceReader::new(rw.bytes.clone())), None).unwrap();
        r.verify_full().unwrap();
        assert_eq!(r.meta().row_count(), 0);
        assert!(r.meta().schema_meta.pack_ids.is_empty());
        assert!(r.scan(&ScanRequest::default()).unwrap().is_empty());
    }

    #[test]
    fn gdpr_rewrite_multi_pack_prunes_unreferenced_packs_and_forces_payload_cols() {
        let schema = Schema::try_new_arc(vec![
            Field::new("user_id", DataType::Utf8, false),
            Field::new("name", DataType::Utf8, false),
            Field::new("input", DataType::Utf8, true),
        ])
        .unwrap();

        // Build two distinct packs: pack_a and pack_b.
        let pack_a_id = [0xA1u8; 16];
        let pack_b_id = [0xB2u8; 16];
        let mut pb_a = ChunkPackBuilder::new(pack_a_id, test_pack_opts());
        let mut pb_b = ChunkPackBuilder::new(pack_b_id, test_pack_opts());

        let keep_a_payload = format!("KEEP_PACK_A_{}", "a".repeat(600));
        let dead_b_payload = format!("ERASE_PACK_B_{}", "b".repeat(600));
        let other_b_payload = format!("OTHER_FILE_KEEP_B_{}", "c".repeat(600));

        let cid_a = pb_a.intern_chunk(keep_a_payload.as_bytes()).unwrap();
        let cid_b_dead = pb_b.intern_chunk(dead_b_payload.as_bytes()).unwrap();
        let cid_b_live = pb_b.intern_chunk(other_b_payload.as_bytes()).unwrap();

        let mut opts_multi = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        opts_multi.payload_cols = vec!["input".to_owned()];

        // File 1 references BOTH pack_a (row 0, surviving) and pack_b (row 1, erased!).
        let batch1 = build_gdpr_batch(&schema, &[("u_keep", "op0", ""), ("u_erase", "op1", "")]);
        let mut refs1 = HashMap::new();
        refs1.insert(
            2usize,
            vec![
                PayloadRefRow::Chunks(vec![(0u16, cid_a)]),
                PayloadRefRow::Chunks(vec![(1u16, cid_b_dead)]),
            ],
        );
        let mut file1_bytes = Vec::new();
        {
            let mut w1 = StatamWriter::new(
                std::io::Cursor::new(&mut file1_bytes),
                Arc::clone(&schema),
                opts_multi.clone(),
                None,
            )
            .unwrap();
            w1.set_pack_ids(vec![pack_a_id, pack_b_id]);
            w1.write_batch_with_prechunked_refs(&batch1, &refs1, None)
                .unwrap();
            w1.finish().unwrap();
        }

        // File 2 keeps cid_b_live alive in pack_b so pack_b is NOT 100%-dead (only rewritten).
        let batch2 = build_gdpr_batch(&schema, &[("u_other", "op2", "")]);
        let mut refs2 = HashMap::new();
        refs2.insert(
            2usize,
            vec![PayloadRefRow::Chunks(vec![(0u16, cid_b_live)])],
        );
        let mut file2_bytes = Vec::new();
        {
            let mut w2 = StatamWriter::new(
                std::io::Cursor::new(&mut file2_bytes),
                Arc::clone(&schema),
                opts_multi,
                None,
            )
            .unwrap();
            w2.set_pack_ids(vec![pack_b_id]);
            w2.write_batch_with_prechunked_refs(&batch2, &refs2, None)
                .unwrap();
            w2.finish().unwrap();
        }

        let mut pack_a_bytes = Vec::new();
        pb_a.finish(&mut pack_a_bytes).unwrap();
        let mut pack_b_bytes = Vec::new();
        pb_b.finish(&mut pack_b_bytes).unwrap();

        // Pass `WriteOptions::default()` (missing `payload_cols: ["input"]`) on file1 to prove
        // GDPR forces `payload_cols` to match input `meta`'s `ExternalPayload` columns!
        let inputs = vec![
            GdprFileInput {
                file_id: "file_multi".to_owned(),
                reader: Arc::new(SliceReader::new(file1_bytes)),
                write_options: Some(WriteOptions::default()),
            },
            GdprFileInput {
                file_id: "file_b_only".to_owned(),
                reader: Arc::new(SliceReader::new(file2_bytes)),
                write_options: None,
            },
        ];
        let mut packs: HashMap<[u8; 16], Arc<dyn RangeReader>> = HashMap::new();
        packs.insert(pack_a_id, Arc::new(SliceReader::new(pack_a_bytes.clone())));
        packs.insert(pack_b_id, Arc::new(SliceReader::new(pack_b_bytes)));

        let report = gdpr_rewrite_day(&inputs, &packs, &|b, row| {
            matches!(
                b.column_by_name("user_id").and_then(|c| c.get_scalar(row).ok()),
                Some(cetra_column::ScalarValue::Utf8(ref u)) if u == "u_erase"
            )
        })
        .unwrap();

        let rw_multi = report
            .rewritten_files
            .iter()
            .find(|f| f.file_id == "file_multi")
            .unwrap();
        let rw_b = report
            .rewritten_files
            .iter()
            .find(|f| f.file_id == "file_b_only")
            .unwrap();

        // file_multi no longer references pack_b at all -> its `pack_ids` must ONLY contain pack_a!
        assert_eq!(rw_multi.stats.pack_ids, vec![pack_a_id]);
        assert!(memmem::find(&rw_multi.bytes, b"ERASE_PACK_B").is_none());

        let mut resolver = InMemoryPackResolver::new();
        resolver.insert_bytes(pack_a_id, pack_a_bytes).unwrap();
        for np in &report.new_packs {
            assert!(memmem::find(&np.bytes, b"ERASE_PACK_B").is_none());
            resolver
                .insert_bytes(np.new_pack_id, np.bytes.clone())
                .unwrap();
        }
        let resolver: Arc<dyn PackResolver> = Arc::new(resolver);

        let r1 = StatamReader::open(
            Arc::new(SliceReader::new(rw_multi.bytes.clone())),
            Some(Arc::clone(&resolver)),
        )
        .unwrap();
        r1.verify_full().unwrap();
        let out1 = r1.scan(&ScanRequest::default()).unwrap();
        assert_eq!(out1[0].num_rows(), 1);
        assert_eq!(
            out1[0].column(2).unwrap().get_scalar(0).unwrap(),
            cetra_column::ScalarValue::Utf8(keep_a_payload)
        );

        let r2 = StatamReader::open(
            Arc::new(SliceReader::new(rw_b.bytes.clone())),
            Some(Arc::clone(&resolver)),
        )
        .unwrap();
        r2.verify_full().unwrap();
        let out2 = r2.scan(&ScanRequest::default()).unwrap();
        assert_eq!(out2[0].num_rows(), 1);
        assert_eq!(
            out2[0].column(2).unwrap().get_scalar(0).unwrap(),
            cetra_column::ScalarValue::Utf8(other_b_payload)
        );
    }

    #[cfg(not(miri))]
    #[test]
    fn gdpr_rewrite_purges_dict_compressed_pack_and_strips_dict_section() {
        let schema = Schema::try_new_arc(vec![
            Field::new("user_id", DataType::Utf8, false),
            Field::new("name", DataType::Utf8, false),
            Field::new("input", DataType::Utf8, true),
        ])
        .unwrap();

        let pack_id = [0xD1u8; 16];
        let pack_opts = PackWriteOptions {
            codec: Codec::Zstd,
            zstd_level: 3,
            target_frame_raw_bytes: 256 * 1024,
            enable_trigram_skip: true,
            compress_toc_lz4: true,
            emit_hash_table: true,
            write_threads: 1,
        };
        let mut pb = ChunkPackBuilder::new_with_dict(pack_id, pack_opts);

        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        opts.payload_cols = vec!["input".to_owned()];
        opts.pack_id = Some(pack_id);

        // Write >= 2 MiB of unique payloads so Zstd dictionary training activates (PACK_FLAG_HAS_DICT).
        let mut rows: Vec<(String, String, String)> = Vec::new();
        let mut expected_survivors: Vec<String> = Vec::new();
        let pad = "structured_dict_gdpr_payload_".repeat(70); // ~2030 B
        for i in 0..1_200u32 {
            let is_erased = i % 5 == 0;
            let user_id = if is_erased {
                "user_erased".to_owned()
            } else {
                format!("user_keep_{}", i % 7)
            };
            let payload = if is_erased {
                format!(
                    "{{\"row\":{i:04},\"secret\":\"ERASED_DICT_PII_{i:04}\",\"pad\":\"{pad}\"}}"
                )
            } else {
                let p = format!(
                    "{{\"row\":{i:04},\"status\":\"KEEP_DICT_OK_{i:04}\",\"pad\":\"{pad}\"}}"
                );
                expected_survivors.push(p.clone());
                p
            };
            rows.push((user_id, format!("span_{i:04}"), payload));
        }

        let row_refs: Vec<(&str, &str, &str)> = rows
            .iter()
            .map(|(u, n, p)| (u.as_str(), n.as_str(), p.as_str()))
            .collect();
        let batch = build_gdpr_batch(&schema, &row_refs);
        let mut file_bytes = Vec::new();
        {
            let mut w = StatamWriter::new(
                std::io::Cursor::new(&mut file_bytes),
                Arc::clone(&schema),
                opts,
                Some(&mut pb),
            )
            .unwrap();
            w.write_batch(&batch).unwrap();
            w.finish().unwrap();
        }

        let mut old_pack_bytes = Vec::new();
        pb.finish(&mut old_pack_bytes).unwrap();
        let old_pack_reader =
            ChunkPackReader::open(Arc::new(SliceReader::new(old_pack_bytes.clone()))).unwrap();
        assert_ne!(
            old_pack_reader.postscript().flags & PACK_FLAG_HAS_DICT,
            0,
            "source >= 2 MiB pack must trigger PACK_FLAG_HAS_DICT"
        );
        assert!(old_pack_reader.postscript().dict_len > 0);
        assert!(
            old_pack_reader
                .toc()
                .frames
                .iter()
                .any(|f| (f.flags & PACK_FRAME_FLAG_USES_DICT) != 0)
        );

        let inputs = vec![GdprFileInput {
            file_id: "dict_file".to_owned(),
            reader: Arc::new(SliceReader::new(file_bytes)),
            write_options: None,
        }];
        let mut packs: HashMap<[u8; 16], Arc<dyn RangeReader>> = HashMap::new();
        packs.insert(pack_id, Arc::new(SliceReader::new(old_pack_bytes)));

        let report = gdpr_rewrite_day(&inputs, &packs, &|b, row| {
            matches!(
                b.column_by_name("user_id").and_then(|c| c.get_scalar(row).ok()),
                Some(cetra_column::ScalarValue::Utf8(ref u)) if u == "user_erased"
            )
        })
        .unwrap();

        assert_eq!(report.new_packs.len(), 1);
        assert_eq!(report.retired_pack_ids, vec![pack_id]);
        let new_pack = &report.new_packs[0];
        let new_pack_reader =
            ChunkPackReader::open(Arc::new(SliceReader::new(new_pack.bytes.clone()))).unwrap();
        assert_eq!(
            new_pack_reader.postscript().flags & PACK_FLAG_HAS_DICT,
            0,
            "rewritten pack must unconditionally strip PACK_FLAG_HAS_DICT"
        );
        assert_eq!(new_pack_reader.postscript().dict_len, 0);
        assert_eq!(new_pack_reader.postscript().dict_off, 0);
        assert!(
            new_pack_reader
                .toc()
                .frames
                .iter()
                .all(|f| (f.flags & PACK_FRAME_FLAG_USES_DICT) == 0)
        );

        // Zero erased bytes in raw and decompressed rewritten pack.
        assert!(memmem::find(&new_pack.bytes, b"ERASED_DICT_PII_").is_none());
        for fid in 0..new_pack_reader.toc().frames.len() as u32 {
            let dec = new_pack_reader.read_frame_decompressed(fid).unwrap();
            assert!(memmem::find(&dec, b"ERASED_DICT_PII_").is_none());
        }

        // Verify rewritten .statam file has zero erased bytes and reads back all survivors bit-exact.
        assert_eq!(report.rewritten_files.len(), 1);
        let rw = &report.rewritten_files[0];
        assert!(memmem::find(&rw.bytes, b"user_erased").is_none());
        assert!(memmem::find(&rw.bytes, b"ERASED_DICT_PII_").is_none());

        let mut resolver = InMemoryPackResolver::new();
        resolver.insert(new_pack.new_pack_id, Arc::new(new_pack_reader));
        let reader = StatamReader::open(
            Arc::new(SliceReader::new(rw.bytes.clone())),
            Some(Arc::new(resolver)),
        )
        .unwrap();
        reader.verify_full().unwrap();

        let out = reader.scan(&ScanRequest::default()).unwrap();
        let mut actual_survivors = Vec::new();
        for b in &out {
            let col = b.column_by_name("input").unwrap();
            for r in 0..b.num_rows() {
                if let cetra_column::ScalarValue::Utf8(s) = col.get_scalar(r).unwrap() {
                    actual_survivors.push(s);
                }
            }
        }
        assert_eq!(actual_survivors, expected_survivors);
    }

    fn build_custom_pack_bytes(
        pack_id: [u8; 16],
        frames_raw: &[Vec<u8>],
        frame_first_and_count: &[(u32, u32)],
        chunks: &[ChunkLoc],
        hashes: &[[u8; 16]],
    ) -> Vec<u8> {
        use crate::format::{
            FORMAT_VERSION, FrameEntry, PACK_HEADER_LEN, PACK_POSTSCRIPT_LEN, PackHeader,
            PackPostscript, write_framed_pack_frame,
        };

        let mut buf = vec![0u8; PACK_HEADER_LEN];
        let mut frame_entries = Vec::with_capacity(frames_raw.len());
        for (raw, &(first_cid, cnt)) in frames_raw.iter().zip(frame_first_and_count.iter()) {
            let offset = buf.len() as u64;
            let mut framed = Vec::new();
            let info = write_framed_pack_frame(raw, Codec::None, 0, 3, &mut framed).unwrap();
            buf.extend_from_slice(&framed);
            frame_entries.push(FrameEntry {
                offset,
                stored_len: info.stored_len,
                raw_len: info.raw_len,
                codec: info.codec,
                flags: 0,
                dict_id: 0,
                first_chunk_id: first_cid,
                chunk_count: cnt,
                filter_off: 0,
                filter_len: 0,
                crc32c: info.crc32c,
            });
        }

        let hash_table_off = if hashes.is_empty() {
            0
        } else {
            let off = buf.len() as u64;
            let start = buf.len();
            for h in hashes {
                buf.extend_from_slice(h);
            }
            let crc = crc32c::crc32c(&buf[start..]);
            buf.extend_from_slice(&crc.to_le_bytes());
            off
        };

        let mut toc_raw = Vec::new();
        for f in &frame_entries {
            toc_raw.extend_from_slice(&f.encode());
        }
        for c in chunks {
            toc_raw.extend_from_slice(&c.encode());
        }
        let toc_crc32c = crc32c::crc32c(&toc_raw);
        let toc_stored_len = (toc_raw.len() + PACK_POSTSCRIPT_LEN) as u32;
        buf.extend_from_slice(&toc_raw);

        let ps = PackPostscript {
            toc_stored_len,
            toc_raw_len: toc_raw.len() as u32,
            format_version: FORMAT_VERSION,
            flags: 0,
            required_features: 0,
            pack_id,
            frame_count: frame_entries.len() as u32,
            chunk_count: chunks.len() as u32,
            hash_table_off,
            trigram_off: 0,
            trigram_len: 0,
            edge_table_len: 0,
            edge_table_off: 0,
            dict_off: 0,
            dict_len: 0,
            enc_envelope_offset: 0,
            enc_envelope_len: 0,
            reserved: 0,
            toc_crc32c,
        };
        let header = PackHeader {
            format_version: FORMAT_VERSION,
            flags: 0,
            required_features: 0,
            pack_id,
            enc_envelope_offset: 0,
            enc_envelope_len: 0,
            reserved: 0,
        };
        buf[0..PACK_HEADER_LEN].copy_from_slice(&header.encode());
        buf.extend_from_slice(&ps.encode());
        buf
    }

    #[test]
    fn b1_gdpr_non_contiguous_pack_erases_pii_and_preserves_survivors() {
        let schema = make_gdpr_schema();
        let pack_id = [0xB1u8; 16];
        let erased_bytes = b"ERASED_NONCONTIG_PII_SECRET_VALUE".repeat(12);
        let live_bytes = b"SURVIVING_NONCONTIG_CLEAN_PAYLOAD".repeat(12);

        // Frame 0 holds erased cid 1; Frame 1 holds live cid 0 -> toc.frames_contiguous == false!
        let chunks = vec![
            ChunkLoc {
                frame_id: 1,
                offset_in_frame: 0,
                raw_len: live_bytes.len() as u32,
            },
            ChunkLoc {
                frame_id: 0,
                offset_in_frame: 0,
                raw_len: erased_bytes.len() as u32,
            },
        ];
        let h0: [u8; 16] = blake3::hash(&live_bytes).as_bytes()[..16]
            .try_into()
            .unwrap();
        let h1: [u8; 16] = blake3::hash(&erased_bytes).as_bytes()[..16]
            .try_into()
            .unwrap();
        let pack_bytes = build_custom_pack_bytes(
            pack_id,
            &[erased_bytes.clone(), live_bytes.clone()],
            &[(0, 1), (1, 1)],
            &chunks,
            &[h0, h1],
        );
        let opened = ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes.clone()))).unwrap();
        assert!(!opened.toc().frames_contiguous);

        let batch = build_gdpr_batch(&schema, &[("u_keep", "op0", ""), ("u_erase", "op1", "")]);
        let mut refs = HashMap::new();
        refs.insert(
            2usize,
            vec![
                PayloadRefRow::Chunks(vec![(0, 0)]),
                PayloadRefRow::Chunks(vec![(0, 1)]),
            ],
        );
        let mut file_bytes = Vec::new();
        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        opts.payload_cols = vec!["input".to_owned()];
        {
            let mut w = StatamWriter::new(
                std::io::Cursor::new(&mut file_bytes),
                Arc::clone(&schema),
                opts,
                None,
            )
            .unwrap();
            w.set_pack_ids(vec![pack_id]);
            w.write_batch_with_prechunked_refs(&batch, &refs, None)
                .unwrap();
            w.finish().unwrap();
        }

        let mut packs: HashMap<[u8; 16], Arc<dyn RangeReader>> = HashMap::new();
        packs.insert(pack_id, Arc::new(SliceReader::new(pack_bytes)));
        let inputs = vec![GdprFileInput {
            file_id: "non_contig".to_owned(),
            reader: Arc::new(SliceReader::new(file_bytes)),
            write_options: None,
        }];

        let report = gdpr_rewrite_day(&inputs, &packs, &|b, r| {
            b.column_by_name("user_id").unwrap().get_scalar(r).unwrap()
                == cetra_column::ScalarValue::Utf8("u_erase".to_owned())
        })
        .unwrap();

        assert_eq!(report.stats.frames_copied_verbatim, 0);
        assert_eq!(report.stats.frames_dropped, 1);
        assert_eq!(report.stats.frames_rewritten, 1);
        assert_eq!(report.stats.chunks_purged, 1);
        assert_eq!(report.stats.bytes_purged, erased_bytes.len() as u64);
        assert_eq!(report.new_packs.len(), 1);
        let new_pack = &report.new_packs[0];
        assert!(memmem::find(&new_pack.bytes, b"ERASED_NONCONTIG_PII").is_none());
        let new_pr =
            ChunkPackReader::open(Arc::new(SliceReader::new(new_pack.bytes.clone()))).unwrap();
        for fid in 0..new_pr.toc().frames.len() as u32 {
            let dec = new_pr.read_frame_decompressed(fid).unwrap();
            assert!(memmem::find(&dec, b"ERASED_NONCONTIG_PII").is_none());
        }

        let mut resolver = InMemoryPackResolver::new();
        resolver.insert(new_pack.new_pack_id, Arc::new(new_pr));
        let reader = StatamReader::open(
            Arc::new(SliceReader::new(report.rewritten_files[0].bytes.clone())),
            Some(Arc::new(resolver)),
        )
        .unwrap();
        let out = reader.scan(&ScanRequest::default()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].num_rows(), 1);
        assert_eq!(
            out[0]
                .column_by_name("input")
                .unwrap()
                .get_scalar(0)
                .unwrap(),
            cetra_column::ScalarValue::Utf8(String::from_utf8(live_bytes).unwrap())
        );
    }

    #[test]
    fn b1_gdpr_overlapping_chunk_slack_frame_purges_erased_bytes() {
        let schema = make_gdpr_schema();
        let pack_id = [0xB2u8; 16];
        let live_half = b"LIVE_CHUNK_BYTES_0123456789_LIVE_CHUNK_BYTES_01234".to_vec();
        assert_eq!(live_half.len(), 50);
        let erased_slack = b"ERASED_SLACK_PII_BYTES_MUST_NOT_SURVIVE_0123456789".to_vec();
        assert_eq!(erased_slack.len(), 50);
        let mut frame0_raw = Vec::with_capacity(100);
        frame0_raw.extend_from_slice(&live_half);
        frame0_raw.extend_from_slice(&erased_slack);
        let frame1_erased = b"ERASED_FRAME1_PAYLOAD_BYTES_1234567890".to_vec();

        // Contiguous pack where Frame 0 has overlapping [0..50], [0..50] leaving [50..100] as unreferenced slack.
        let chunks = vec![
            ChunkLoc {
                frame_id: 0,
                offset_in_frame: 0,
                raw_len: 50,
            },
            ChunkLoc {
                frame_id: 0,
                offset_in_frame: 0,
                raw_len: 50,
            },
            ChunkLoc {
                frame_id: 1,
                offset_in_frame: 0,
                raw_len: frame1_erased.len() as u32,
            },
        ];
        let h_live: [u8; 16] = blake3::hash(&live_half).as_bytes()[..16]
            .try_into()
            .unwrap();
        let h_dead: [u8; 16] = blake3::hash(&frame1_erased).as_bytes()[..16]
            .try_into()
            .unwrap();
        let pack_bytes = build_custom_pack_bytes(
            pack_id,
            &[frame0_raw, frame1_erased],
            &[(0, 2), (2, 1)],
            &chunks,
            &[h_live, h_live, h_dead],
        );
        let opened = ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes.clone()))).unwrap();
        assert!(opened.toc().frames_contiguous);

        let batch = build_gdpr_batch(
            &schema,
            &[
                ("u_keep0", "op0", ""),
                ("u_keep1", "op1", ""),
                ("u_erase", "op2", ""),
            ],
        );
        let mut refs = HashMap::new();
        refs.insert(
            2usize,
            vec![
                PayloadRefRow::Chunks(vec![(0, 0)]),
                PayloadRefRow::Chunks(vec![(0, 1)]),
                PayloadRefRow::Chunks(vec![(0, 2)]),
            ],
        );
        let mut file_bytes = Vec::new();
        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        opts.payload_cols = vec!["input".to_owned()];
        {
            let mut w = StatamWriter::new(
                std::io::Cursor::new(&mut file_bytes),
                Arc::clone(&schema),
                opts,
                None,
            )
            .unwrap();
            w.set_pack_ids(vec![pack_id]);
            w.write_batch_with_prechunked_refs(&batch, &refs, None)
                .unwrap();
            w.finish().unwrap();
        }

        let mut packs: HashMap<[u8; 16], Arc<dyn RangeReader>> = HashMap::new();
        packs.insert(pack_id, Arc::new(SliceReader::new(pack_bytes)));
        let inputs = vec![GdprFileInput {
            file_id: "slack".to_owned(),
            reader: Arc::new(SliceReader::new(file_bytes)),
            write_options: None,
        }];

        let report = gdpr_rewrite_day(&inputs, &packs, &|b, r| {
            b.column_by_name("user_id").unwrap().get_scalar(r).unwrap()
                == cetra_column::ScalarValue::Utf8("u_erase".to_owned())
        })
        .unwrap();

        assert_eq!(report.stats.frames_copied_verbatim, 0);
        assert_eq!(report.stats.frames_rewritten, 1);
        assert_eq!(report.stats.frames_dropped, 1);
        let new_pack = &report.new_packs[0];
        assert!(memmem::find(&new_pack.bytes, b"ERASED_SLACK_PII").is_none());
        let new_pr =
            ChunkPackReader::open(Arc::new(SliceReader::new(new_pack.bytes.clone()))).unwrap();
        for fid in 0..new_pr.toc().frames.len() as u32 {
            let dec = new_pr.read_frame_decompressed(fid).unwrap();
            assert!(memmem::find(&dec, b"ERASED_SLACK_PII").is_none());
        }

        let mut resolver = InMemoryPackResolver::new();
        resolver.insert(new_pack.new_pack_id, Arc::new(new_pr));
        let reader = StatamReader::open(
            Arc::new(SliceReader::new(report.rewritten_files[0].bytes.clone())),
            Some(Arc::new(resolver)),
        )
        .unwrap();
        let out = reader.scan(&ScanRequest::default()).unwrap();
        assert_eq!(out[0].num_rows(), 2);
        let expected_str = String::from_utf8(live_half).unwrap();
        for row in 0..2 {
            assert_eq!(
                out[0]
                    .column_by_name("input")
                    .unwrap()
                    .get_scalar(row)
                    .unwrap(),
                cetra_column::ScalarValue::Utf8(expected_str.clone())
            );
        }
    }

    #[test]
    fn b2_gdpr_duplicate_chunk_across_single_and_multi_chunk_frames_preserves_boundary_search() {
        let schema = make_gdpr_schema();
        let pack_id = [0xB3u8; 16];
        let chunk_x = format!("{}BOUNDARY_", "x".repeat(300));
        let chunk_y = format!("SPAN_NEEDLE{}", "y".repeat(300));
        let chunk_z = format!("ERASED_DEAD_CHUNK_{}", "z".repeat(300));

        // Build pack: Frame 0 = [X] (single-chunk, empty edge via bypass), Frame 1 = [X, Y, Z] (mixed).
        let mut pb = ChunkPackBuilder::new(
            pack_id,
            PackWriteOptions {
                codec: default_gdpr_pack_codec(),
                target_frame_raw_bytes: 320,
                enable_trigram_skip: true,
                compress_toc_lz4: false,
                emit_hash_table: true,
                write_threads: 1,
                zstd_level: 3,
            },
        );
        let cid_x_single = pb
            .append_raw_bypass_with_edge(chunk_x.as_bytes(), false)
            .unwrap();
        let cid_x_multi = pb
            .append_raw_bypass_with_edge(chunk_x.as_bytes(), true)
            .unwrap();
        let cid_y = pb
            .append_raw_bypass_with_edge(chunk_y.as_bytes(), true)
            .unwrap();
        let cid_z_dead = pb
            .append_raw_bypass_with_edge(chunk_z.as_bytes(), false)
            .unwrap();
        let mut pack_bytes = Vec::new();
        pb.finish(&mut pack_bytes).unwrap();

        let batch = build_gdpr_batch(
            &schema,
            &[
                ("u_keep_single", "op0", ""),
                ("u_keep_multi", "op1", ""),
                ("u_erase", "op2", ""),
            ],
        );
        let mut refs = HashMap::new();
        refs.insert(
            2usize,
            vec![
                PayloadRefRow::Chunks(vec![(0, cid_x_single)]),
                PayloadRefRow::Chunks(vec![(0, cid_x_multi), (0, cid_y)]),
                PayloadRefRow::Chunks(vec![(0, cid_z_dead)]),
            ],
        );
        let mut file_bytes = Vec::new();
        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        opts.payload_cols = vec!["input".to_owned()];
        {
            let mut w = StatamWriter::new(
                std::io::Cursor::new(&mut file_bytes),
                Arc::clone(&schema),
                opts,
                None,
            )
            .unwrap();
            w.set_pack_ids(vec![pack_id]);
            w.write_batch_with_prechunked_refs(&batch, &refs, None)
                .unwrap();
            w.finish().unwrap();
        }

        let mut packs: HashMap<[u8; 16], Arc<dyn RangeReader>> = HashMap::new();
        packs.insert(pack_id, Arc::new(SliceReader::new(pack_bytes)));
        let inputs = vec![GdprFileInput {
            file_id: "b2_dup".to_owned(),
            reader: Arc::new(SliceReader::new(file_bytes)),
            write_options: None,
        }];

        let report = gdpr_rewrite_day(&inputs, &packs, &|b, r| {
            b.column_by_name("user_id").unwrap().get_scalar(r).unwrap()
                == cetra_column::ScalarValue::Utf8("u_erase".to_owned())
        })
        .unwrap();

        let new_pack = &report.new_packs[0];
        let mut resolver = InMemoryPackResolver::new();
        resolver
            .insert_bytes(new_pack.new_pack_id, new_pack.bytes.clone())
            .unwrap();
        let reader = StatamReader::open(
            Arc::new(SliceReader::new(report.rewritten_files[0].bytes.clone())),
            Some(Arc::new(resolver)),
        )
        .unwrap();

        for ci in [false, true] {
            let needle = if ci {
                b"boundary_span_needle".to_vec()
            } else {
                b"BOUNDARY_SPAN_NEEDLE".to_vec()
            };
            let hits = reader
                .scan(&ScanRequest {
                    substrings: vec![crate::reader::SubstringQuery {
                        column: "input".to_owned(),
                        needle,
                        case_insensitive: ci,
                    }],
                    ..ScanRequest::default()
                })
                .unwrap();
            let matched: usize = hits.iter().map(Batch::num_rows).sum();
            assert_eq!(
                matched, 1,
                "cross-chunk LIKE/ILIKE (ci={ci}) must find X||Y"
            );
        }
    }

    #[test]
    fn m8_gdpr_rejects_missing_pack_referenced_only_by_erased_rows() {
        let schema = make_gdpr_schema();
        let pack_id = [0x88u8; 16];
        let mut pb = ChunkPackBuilder::new(pack_id, PackWriteOptions::default());
        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        opts.payload_cols = vec!["input".to_owned()];
        opts.pack_id = Some(pack_id);
        let big = "ERASED_ONLY_PACK_PAYLOAD_".repeat(20);
        let batch = build_gdpr_batch(&schema, &[("u_erase", "op0", &big)]);
        let mut file_bytes = Vec::new();
        {
            let mut w = StatamWriter::new(
                std::io::Cursor::new(&mut file_bytes),
                Arc::clone(&schema),
                opts,
                Some(&mut pb),
            )
            .unwrap();
            w.write_batch(&batch).unwrap();
            w.finish().unwrap();
        }

        let inputs = vec![GdprFileInput {
            file_id: "all_erased".to_owned(),
            reader: Arc::new(SliceReader::new(file_bytes)),
            write_options: None,
        }];
        let empty_packs: HashMap<[u8; 16], Arc<dyn RangeReader>> = HashMap::new();
        let err = gdpr_rewrite_day(&inputs, &empty_packs, &|_, _| true).unwrap_err();
        assert!(matches!(err, StatamError::MissingPack { pack_id: pid } if pid == pack_id));
    }

    #[test]
    fn b7_gdpr_streams_multi_frame_l0_rewrite_bit_exact() {
        let schema = make_gdpr_schema();
        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
        opts.payload_cols = vec!["input".to_owned()];

        // 12 rows x ~20 KiB = ~240 KiB -> spans multiple 64 KiB L0 frames in 1 RG.
        let mut owned = Vec::new();
        for i in 0..12usize {
            let uid = if i % 3 == 0 {
                format!("u_erase_{i}")
            } else {
                format!("u_keep_{i}")
            };
            let payload = if i % 3 == 0 {
                format!("ERASED_L0_STREAM_{i:02}_").repeat(900)
            } else {
                format!("SURVIVING_L0_STREAM_{i:02}_").repeat(900)
            };
            owned.push((uid, format!("op_{i}"), payload));
        }
        let rows_ref: Vec<(&str, &str, &str)> = owned
            .iter()
            .map(|(u, o, p)| (u.as_str(), o.as_str(), p.as_str()))
            .collect();
        let batch = build_gdpr_batch(&schema, &rows_ref);
        let mut file_bytes = Vec::new();
        {
            let mut w = StatamWriter::new(
                std::io::Cursor::new(&mut file_bytes),
                Arc::clone(&schema),
                opts,
                None,
            )
            .unwrap();
            w.write_batch(&batch).unwrap();
            w.finish().unwrap();
        }

        let inputs = vec![GdprFileInput {
            file_id: "l0_multi_frame".to_owned(),
            reader: Arc::new(SliceReader::new(file_bytes)),
            write_options: None,
        }];
        let empty_packs = HashMap::new();
        let report = gdpr_rewrite_day(&inputs, &empty_packs, &|b, r| {
            matches!(
                b.column_by_name("user_id").unwrap().get_scalar(r).unwrap(),
                cetra_column::ScalarValue::Utf8(s) if s.starts_with("u_erase_")
            )
        })
        .unwrap();

        assert_eq!(report.rewritten_files.len(), 1);
        let rw = &report.rewritten_files[0];
        assert!(memmem::find(&rw.bytes, b"ERASED_L0_STREAM_").is_none());
        let reader =
            StatamReader::open(Arc::new(SliceReader::new(rw.bytes.clone())), None).unwrap();
        reader.verify_full().unwrap();
        let out = reader.scan(&ScanRequest::default()).unwrap();
        let expected: Vec<String> = owned
            .iter()
            .filter(|(u, _, _)| !u.starts_with("u_erase_"))
            .map(|(_, _, p)| p.clone())
            .collect();
        let mut actual = Vec::new();
        for b in &out {
            let col = b.column_by_name("input").unwrap();
            for r in 0..b.num_rows() {
                if let cetra_column::ScalarValue::Utf8(s) = col.get_scalar(r).unwrap() {
                    actual.push(s);
                }
            }
        }
        assert_eq!(actual, expected);
    }
}
