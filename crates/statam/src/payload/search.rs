//! Exact substring search (`LIKE` / `ILIKE`) over inline payloads and `.stpk` chunk packs.

use std::collections::{BTreeSet, HashMap, HashSet};

use cetra_column::Bitmap;
use memchr::memmem;

use crate::StatamError;
use crate::format::MAX_ROW_PAYLOAD_BYTES;
use crate::payload::PayloadRefRow;
use crate::payload::pack::ChunkPackReader;
use crate::simd::trigram::{TrigramBloom, ascii_lowercase_into, extract_trigram_hashes};

/// Maximum needle byte length guaranteed to span at most two adjacent `>= 256 B` chunks
/// and lie completely inside `tail_63(left) || head_63(right)` when crossing a boundary.
const MAX_EDGE_NEEDLE_LEN: usize = 64;

enum NeedleMatcher {
    Exact(memmem::Finder<'static>),
    AsciiIlike {
        finder: memmem::Finder<'static>,
        scratch: Vec<u8>,
    },
    UnicodeIlike {
        finder: memmem::Finder<'static>,
        needle_raw_lower: Vec<u8>,
        scratch: Vec<u8>,
    },
}

impl NeedleMatcher {
    fn new(needle: &[u8], case_insensitive: bool) -> Self {
        if !case_insensitive {
            return Self::Exact(memmem::Finder::new(needle).into_owned());
        }
        if needle.is_ascii() {
            let mut lowered = Vec::new();
            ascii_lowercase_into(needle, &mut lowered);
            return Self::AsciiIlike {
                finder: memmem::Finder::new(&lowered).into_owned(),
                scratch: Vec::new(),
            };
        }

        let mut ascii_lowered = Vec::new();
        ascii_lowercase_into(needle, &mut ascii_lowered);
        match std::str::from_utf8(needle) {
            Ok(s) => {
                let uni_lower = s.to_lowercase();
                Self::UnicodeIlike {
                    finder: memmem::Finder::new(uni_lower.as_bytes()).into_owned(),
                    needle_raw_lower: ascii_lowered,
                    scratch: Vec::new(),
                }
            }
            Err(_) => Self::AsciiIlike {
                finder: memmem::Finder::new(&ascii_lowered).into_owned(),
                scratch: Vec::new(),
            },
        }
    }

    fn matches(&mut self, haystack: &[u8]) -> bool {
        match self {
            Self::Exact(finder) => finder.find(haystack).is_some(),
            Self::AsciiIlike { finder, scratch } => {
                ascii_lowercase_into(haystack, scratch);
                finder.find(scratch).is_some()
            }
            Self::UnicodeIlike {
                finder,
                needle_raw_lower,
                scratch,
            } => {
                if let Ok(s) = std::str::from_utf8(haystack) {
                    let lowered = s.to_lowercase();
                    finder.find(lowered.as_bytes()).is_some()
                } else {
                    // Fallback for invalid UTF-8 slices (e.g. clipped boundary edge):
                    // try lossy UTF-8 Unicode lowercase AND raw ASCII lowercase.
                    let lossy = String::from_utf8_lossy(haystack).to_lowercase();
                    if finder.find(lossy.as_bytes()).is_some() {
                        return true;
                    }
                    ascii_lowercase_into(haystack, scratch);
                    memmem::find(scratch, needle_raw_lower).is_some()
                }
            }
        }
    }
}

/// Searches all unique chunks inside `pack` for `needle` (interior chunk matches only).
///
/// Uses the pack's `TrigramSkipSection` for `LIKE` and ASCII trigrams of `ILIKE`
/// whenever `needle` yields at least 1 trigram; otherwise scans all frames.
pub fn search_pack_chunks(
    pack: &ChunkPackReader,
    needle: &[u8],
    case_insensitive: bool,
) -> Result<Vec<u32>, StatamError> {
    let chunk_count = pack.toc().chunks.len();
    if chunk_count == 0 {
        return Ok(Vec::new());
    }
    if needle.is_empty() {
        return Ok((0..chunk_count as u32).collect());
    }

    let all_ids: Vec<u32> = (0..chunk_count as u32).collect();
    let candidate_ids = filter_candidates_by_trigram(pack, &all_ids, needle, case_insensitive)?;
    if candidate_ids.is_empty() {
        return Ok(Vec::new());
    }

    let chunk_map = pack.read_chunks(&candidate_ids)?;
    let mut matcher = NeedleMatcher::new(needle, case_insensitive);
    let mut matched = Vec::new();
    for cid in candidate_ids {
        let bytes = chunk_map.get(&cid).ok_or(StatamError::Corrupt {
            reason: "missing fetched chunk during pack search",
        })?;
        if matcher.matches(bytes) {
            matched.push(cid);
        }
    }
    Ok(matched)
}

/// Exact row-level substring search (`LIKE` / `ILIKE`) across inline payloads and `.stpk` chunks.
///
/// Guarantees **zero false negatives** and **zero false positives**:
/// - Empty `needle` matches all non-null rows.
/// - Inline payloads (`<= 256 B`) are scanned directly.
/// - `needle.len() <= 64`: Step A prunes frames via ASCII trigram blooms and scans each
///   surviving unique chunk once; Step B checks every distinct adjacent chunk pair
///   `(left, right)` in surviving rows using `EdgeTable`'s `tail_63(left) || head_63(right)`.
/// - `needle.len() > 64`: deduplicates unique chunk-ref sequences, fetches unique chunks
///   in one wave, reconstructs each unique sequence once, and runs exact search.
pub fn search_pack_rows(
    pack: &ChunkPackReader,
    row_refs: &[PayloadRefRow],
    pack_idx: u16,
    needle: &[u8],
    case_insensitive: bool,
) -> Result<Bitmap, StatamError> {
    search_pack_rows_impl(pack, row_refs, None, pack_idx, needle, case_insensitive)
}

/// Exact row-level substring search restricted to rows active in `active_mask`,
/// avoiding per-row `PayloadRefRow` cloning in callers.
pub(crate) fn search_pack_rows_masked(
    pack: &ChunkPackReader,
    row_refs: &[PayloadRefRow],
    active_mask: &Bitmap,
    pack_idx: u16,
    needle: &[u8],
    case_insensitive: bool,
) -> Result<Bitmap, StatamError> {
    search_pack_rows_impl(
        pack,
        row_refs,
        Some(active_mask),
        pack_idx,
        needle,
        case_insensitive,
    )
}

fn visit_active_rows(
    row_count: usize,
    active_mask: Option<&Bitmap>,
    mut f: impl FnMut(usize) -> Result<(), StatamError>,
) -> Result<(), StatamError> {
    if let Some(bm) = active_mask {
        for r in bm.iter_ones() {
            if r >= row_count {
                break;
            }
            f(r)?;
        }
    } else {
        for r in 0..row_count {
            f(r)?;
        }
    }
    Ok(())
}

fn search_pack_rows_impl(
    pack: &ChunkPackReader,
    row_refs: &[PayloadRefRow],
    active_mask: Option<&Bitmap>,
    pack_idx: u16,
    needle: &[u8],
    case_insensitive: bool,
) -> Result<Bitmap, StatamError> {
    let mut results = Bitmap::new_zeroed(row_refs.len());
    if active_mask.is_some_and(Bitmap::all_zeros) {
        return Ok(results);
    }

    if needle.is_empty() {
        visit_active_rows(row_refs.len(), active_mask, |idx| {
            if !matches!(row_refs[idx], PayloadRefRow::Null) {
                let _ = results.set(idx, true);
            }
            Ok(())
        })?;
        return Ok(results);
    }

    let mut matcher = NeedleMatcher::new(needle, case_insensitive);
    let pack_chunk_count = pack.toc().chunks.len();

    if pack_short_circuits_all(pack, needle, case_insensitive)? {
        visit_active_rows(row_refs.len(), active_mask, |row_idx| {
            match &row_refs[row_idx] {
                PayloadRefRow::Null => {}
                PayloadRefRow::Inline(bytes) => {
                    if matcher.matches(bytes) {
                        let _ = results.set(row_idx, true);
                    }
                }
                PayloadRefRow::Chunks(refs) => {
                    for &(p, cid) in refs {
                        if p == pack_idx && (cid as usize) >= pack_chunk_count {
                            return Err(StatamError::Corrupt {
                                reason: "payload ref chunk_id out of pack bounds",
                            });
                        }
                    }
                }
            }
            Ok(())
        })?;
        return Ok(results);
    }

    let mut chunk_rows = Vec::new();
    let mut needed_chunks = BTreeSet::new();

    visit_active_rows(row_refs.len(), active_mask, |row_idx| {
        match &row_refs[row_idx] {
            PayloadRefRow::Null => {}
            PayloadRefRow::Inline(bytes) => {
                if matcher.matches(bytes) {
                    let _ = results.set(row_idx, true);
                }
            }
            PayloadRefRow::Chunks(refs) => {
                let mut has_pack = false;
                for &(p, cid) in refs {
                    if p == pack_idx {
                        if (cid as usize) >= pack_chunk_count {
                            return Err(StatamError::Corrupt {
                                reason: "payload ref chunk_id out of pack bounds",
                            });
                        }
                        needed_chunks.insert(cid);
                        has_pack = true;
                    }
                }
                if has_pack {
                    chunk_rows.push(row_idx);
                }
            }
        }
        Ok(())
    })?;

    if chunk_rows.is_empty() {
        return Ok(results);
    }

    if needle.len() <= MAX_EDGE_NEEDLE_LEN {
        search_short_needle_rows(
            pack,
            row_refs,
            pack_idx,
            needle,
            case_insensitive,
            &mut matcher,
            &chunk_rows,
            &needed_chunks,
            &mut results,
        )?;
    } else {
        search_long_needle_rows(
            pack,
            row_refs,
            pack_idx,
            &mut matcher,
            &chunk_rows,
            &needed_chunks,
            &mut results,
        )?;
    }

    Ok(results)
}

#[allow(clippy::too_many_arguments)]
fn search_short_needle_rows(
    pack: &ChunkPackReader,
    row_refs: &[PayloadRefRow],
    pack_idx: u16,
    needle: &[u8],
    case_insensitive: bool,
    matcher: &mut NeedleMatcher,
    chunk_rows: &[usize],
    needed_chunks: &BTreeSet<u32>,
    results: &mut Bitmap,
) -> Result<(), StatamError> {
    let skip_boundary = if needle.len() >= 5
        && (!case_insensitive || needle.is_ascii())
        && pack.toc().postscript.trigram_len > 0
    {
        let ts = pack.trigram_section_cached()?;
        pack_rejects_boundary_pigeonhole(ts.edge_blooms.as_ref(), needle, case_insensitive)
    } else {
        false
    };

    // Step A: Interior matches via trigram bloom pruning + single-pass unique chunk scan.
    let all_needed: Vec<u32> = needed_chunks.iter().copied().collect();
    let surviving_chunks =
        filter_candidates_by_trigram(pack, &all_needed, needle, case_insensitive)?;

    if surviving_chunks.is_empty() && skip_boundary {
        return Ok(());
    }

    let mut matched_chunks = HashSet::new();
    if !surviving_chunks.is_empty() {
        pack.scan_chunks_in_frames(&surviving_chunks, |cid, bytes| {
            if matcher.matches(bytes) {
                matched_chunks.insert(cid);
            }
        })?;
    }

    if skip_boundary {
        if !matched_chunks.is_empty() {
            for &row_idx in chunk_rows {
                if let PayloadRefRow::Chunks(refs) = &row_refs[row_idx]
                    && refs
                        .iter()
                        .any(|&(p, cid)| p == pack_idx && matched_chunks.contains(&cid))
                {
                    let _ = results.set(row_idx, true);
                }
            }
        }
        return Ok(());
    }

    let mut remaining_rows = Vec::new();
    for &row_idx in chunk_rows {
        if let PayloadRefRow::Chunks(refs) = &row_refs[row_idx] {
            let hit = refs
                .iter()
                .any(|&(p, cid)| p == pack_idx && matched_chunks.contains(&cid));
            if hit {
                let _ = results.set(row_idx, true);
            } else {
                remaining_rows.push(row_idx);
            }
        }
    }

    if remaining_rows.is_empty() {
        return Ok(());
    }

    // Step B: Cross-chunk boundary matches using EdgeTable (0 extra data frame reads).
    let mut distinct_pairs = HashSet::new();
    for &row_idx in &remaining_rows {
        if let PayloadRefRow::Chunks(refs) = &row_refs[row_idx] {
            for w in refs.windows(2) {
                if w[0].0 == pack_idx && w[1].0 == pack_idx {
                    distinct_pairs.insert((w[0].1, w[1].1));
                }
            }
        }
    }

    if distinct_pairs.is_empty() {
        return Ok(());
    }

    let edges = pack.read_edge_table()?;
    let mut matched_pairs = HashSet::new();
    let mut combined = Vec::with_capacity(126);
    let is_unicode_ilike = matches!(matcher, NeedleMatcher::UnicodeIlike { .. });

    for (left_cid, right_cid) in distinct_pairs {
        let left_tail = &edges
            .get(left_cid as usize)
            .ok_or(StatamError::Corrupt {
                reason: "left chunk_id missing from edge table",
            })?
            .1;
        let right_head = &edges
            .get(right_cid as usize)
            .ok_or(StatamError::Corrupt {
                reason: "right chunk_id missing from edge table",
            })?
            .0;

        combined.clear();
        if is_unicode_ilike {
            combined.extend_from_slice(trim_leading_utf8_continuations(left_tail));
            combined.extend_from_slice(trim_trailing_incomplete_utf8(right_head));
        } else {
            combined.extend_from_slice(left_tail);
            combined.extend_from_slice(right_head);
        }
        if matcher.matches(&combined) {
            matched_pairs.insert((left_cid, right_cid));
        }
    }

    if !matched_pairs.is_empty() {
        for &row_idx in &remaining_rows {
            if let PayloadRefRow::Chunks(refs) = &row_refs[row_idx] {
                let hit = refs.windows(2).any(|w| {
                    w[0].0 == pack_idx
                        && w[1].0 == pack_idx
                        && matched_pairs.contains(&(w[0].1, w[1].1))
                });
                if hit {
                    let _ = results.set(row_idx, true);
                }
            }
        }
    }

    Ok(())
}

fn search_long_needle_rows(
    pack: &ChunkPackReader,
    row_refs: &[PayloadRefRow],
    pack_idx: u16,
    matcher: &mut NeedleMatcher,
    chunk_rows: &[usize],
    needed_chunks: &BTreeSet<u32>,
    results: &mut Bitmap,
) -> Result<(), StatamError> {
    let all_needed: Vec<u32> = needed_chunks.iter().copied().collect();
    let fetched = pack.read_chunks(&all_needed)?;

    let mut seq_cache: HashMap<&[(u16, u32)], bool> = HashMap::new();
    let mut scratch_buf = Vec::new();

    for &row_idx in chunk_rows {
        let PayloadRefRow::Chunks(refs) = &row_refs[row_idx] else {
            continue;
        };
        if let Some(&cached) = seq_cache.get(refs.as_slice()) {
            if cached {
                let _ = results.set(row_idx, true);
            }
            continue;
        }

        // Reconstruct contiguous runs belonging to `pack_idx` and search each run.
        let mut total_row_bytes: usize = 0;
        let mut hit = false;
        scratch_buf.clear();

        for &(p, cid) in refs {
            if p == pack_idx {
                let chunk = fetched.get(&cid).ok_or(StatamError::Corrupt {
                    reason: "missing fetched chunk during long-needle reconstruction",
                })?;
                total_row_bytes = total_row_bytes.checked_add(chunk.len()).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "reconstructed row payload overflows usize",
                    },
                )?;
                if total_row_bytes > MAX_ROW_PAYLOAD_BYTES {
                    return Err(StatamError::BoundsExceeded {
                        reason: "reconstructed row payload exceeds MAX_ROW_PAYLOAD_BYTES",
                    });
                }
                scratch_buf.extend_from_slice(chunk);
            } else if !scratch_buf.is_empty() {
                if matcher.matches(&scratch_buf) {
                    hit = true;
                    break;
                }
                scratch_buf.clear();
            }
        }
        if !hit && !scratch_buf.is_empty() {
            hit = matcher.matches(&scratch_buf);
        }

        seq_cache.insert(refs.as_slice(), hit);
        if hit {
            let _ = results.set(row_idx, true);
        }
    }

    Ok(())
}

fn trim_leading_utf8_continuations(bytes: &[u8]) -> &[u8] {
    let mut start = 0usize;
    while start < bytes.len() && (bytes[start] & 0xC0) == 0x80 {
        start += 1;
    }
    &bytes[start..]
}

fn trim_trailing_incomplete_utf8(bytes: &[u8]) -> &[u8] {
    let len = bytes.len();
    if len == 0 {
        return bytes;
    }
    let mut cont = 0usize;
    while cont < 3 && cont < len && (bytes[len - 1 - cont] & 0xC0) == 0x80 {
        cont += 1;
    }
    if cont == len {
        return bytes;
    }
    let lead_idx = len - 1 - cont;
    let lead = bytes[lead_idx];
    let expected_len = if lead < 0x80 {
        1
    } else if matches!(lead, 0xC2..=0xDF) {
        2
    } else if matches!(lead, 0xE0..=0xEF) {
        3
    } else if matches!(lead, 0xF0..=0xF4) {
        4
    } else {
        return bytes;
    };
    if cont + 1 < expected_len {
        &bytes[..lead_idx]
    } else {
        bytes
    }
}

pub(crate) fn pack_rejects_boundary_pigeonhole(
    edge_blooms: Option<&(TrigramBloom, TrigramBloom)>,
    needle: &[u8],
    case_insensitive: bool,
) -> bool {
    if needle.len() < 5
        || needle.len() > MAX_EDGE_NEEDLE_LEN
        || (case_insensitive && !needle.is_ascii())
    {
        return false;
    }
    let Some((tail_bloom, head_bloom)) = edge_blooms else {
        return false;
    };
    let n = needle.len();
    let half = n.div_ceil(2);
    let mut prefix_hashes = Vec::new();
    extract_trigram_hashes(&needle[..half], case_insensitive, &mut prefix_hashes);
    let mut suffix_hashes = Vec::new();
    extract_trigram_hashes(&needle[n - half..], case_insensitive, &mut suffix_hashes);
    let tail_may = tail_bloom.may_contain_hashes(&prefix_hashes);
    let head_may = head_bloom.may_contain_hashes(&suffix_hashes);
    !tail_may && !head_may
}

pub(crate) fn pack_short_circuits_all(
    pack: &ChunkPackReader,
    needle: &[u8],
    case_insensitive: bool,
) -> Result<bool, StatamError> {
    if needle.len() < 5
        || needle.len() > MAX_EDGE_NEEDLE_LEN
        || (case_insensitive && !needle.is_ascii())
        || pack.toc().postscript.trigram_len == 0
    {
        return Ok(false);
    }

    let ts = pack.trigram_section_cached()?;
    if !pack_rejects_boundary_pigeonhole(ts.edge_blooms.as_ref(), needle, case_insensitive) {
        return Ok(false);
    }

    let mut hashes = Vec::new();
    extract_trigram_hashes(needle, case_insensitive, &mut hashes);
    if hashes.is_empty() {
        return Ok(false);
    }

    let any_frame_may = ts
        .frame_blooms
        .iter()
        .any(|opt| opt.as_ref().is_none_or(|b| b.may_contain_hashes(&hashes)));
    Ok(!any_frame_may)
}

fn filter_candidates_by_trigram(
    pack: &ChunkPackReader,
    chunk_ids: &[u32],
    needle: &[u8],
    case_insensitive: bool,
) -> Result<Vec<u32>, StatamError> {
    if chunk_ids.is_empty() {
        return Ok(Vec::new());
    }
    if pack.toc().postscript.trigram_len == 0 {
        return Ok(chunk_ids.to_vec());
    }
    // Unicode `ILIKE` with non-ASCII needles can expand/fold characters (e.g. `İ -> i\u{0307}`,
    // `K -> k`) via `str::to_lowercase()`, so skip trigram bloom pruning completely.
    if case_insensitive && !needle.is_ascii() {
        return Ok(chunk_ids.to_vec());
    }

    let mut hashes = Vec::new();
    extract_trigram_hashes(needle, case_insensitive, &mut hashes);
    if hashes.is_empty() {
        return Ok(chunk_ids.to_vec());
    }

    let blooms = &pack.trigram_section_cached()?.frame_blooms;
    let frame_matches: Vec<bool> = blooms
        .iter()
        .map(|opt| match opt {
            None => true,
            Some(bloom) => bloom.may_contain_hashes(&hashes),
        })
        .collect();

    let mut surviving = Vec::with_capacity(chunk_ids.len());
    for &cid in chunk_ids {
        let loc = pack
            .toc()
            .chunks
            .get(cid as usize)
            .ok_or(StatamError::Corrupt {
                reason: "chunk_id out of bounds during trigram filter",
            })?;
        if frame_matches
            .get(loc.frame_id as usize)
            .copied()
            .unwrap_or(true)
        {
            surviving.push(cid);
        }
    }
    Ok(surviving)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::format::Codec;
    use crate::io::{SimulatedS3Config, SimulatedS3Reader, SliceReader};
    use crate::payload::chunker::chunk_payload;
    use crate::payload::pack::{ChunkPackBuilder, PackWriteOptions};

    fn test_codec() -> Codec {
        #[cfg(miri)]
        {
            Codec::Lz4
        }
        #[cfg(not(miri))]
        {
            Codec::Zstd
        }
    }

    #[test]
    fn substring_search_exact_interior_boundary_absent_long_and_unicode_ilike() {
        let opts = PackWriteOptions {
            codec: test_codec(),
            zstd_level: 3,
            target_frame_raw_bytes: 16 * 1024,
            enable_trigram_skip: true,
            compress_toc_lz4: true,
        };
        let mut builder = ChunkPackBuilder::new([0x55u8; 16], opts);

        // Row 0: Null
        // Row 1: Inline <= 256 B with exact match
        // Row 2: Chunked payload with interior match ("TOKEN_INTERIOR_99") + Unicode ("Café_ÜBER_Secret")
        // Row 3: Chunked payload where needle ("CROSS_BOUNDARY_NEEDLE_42") spans chunk boundary!
        // Row 4: Chunked payload with long (> 64 B) needle spanning chunk boundary!
        // Row 5: Unrelated large chunks (> 2560 B compressed) to exercise trigram frame pruning.

        let pad_a = "a".repeat(300);
        let pad_b = "b".repeat(300);

        // Construct Row 2 (>= 512 B JSON array -> multiple chunks)
        let row2_bytes = format!(
            "[{{\"role\":\"user\",\"content\":\"{pad_a} TOKEN_INTERIOR_99 {pad_a}\"}},{{\"role\":\"assistant\",\"content\":\"{pad_b} Café_ÜBER_Secret {pad_b}\"}}]"
        );

        // Construct Row 3 with a forced chunk boundary right in the middle of "CROSS_BOUNDARY_NEEDLE_42"
        let r3_left = format!("{{\"k\":\"{}\"CROSS_BOUNDARY_", "p".repeat(280));
        let r3_right = format!("NEEDLE_42\"{}\"}}", "q".repeat(280));
        assert!(r3_left.len() >= 256);
        assert!(r3_right.len() >= 256);

        // Construct Row 4 with a > 64 B needle spanning two chunks
        let long_needle =
            "LONG_SPANNING_NEEDLE_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ_0123456789_END";
        assert!(long_needle.len() > 64);
        let r4_left = format!(
            "{{\"m\":\"{}LONG_SPANNING_NEEDLE_0123456789_",
            "r".repeat(280)
        );
        let r4_right = format!(
            "ABCDEFGHIJKLMNOPQRSTUVWXYZ_0123456789_END{}\"}}",
            "s".repeat(280)
        );
        assert!(r4_left.len() >= 256);
        assert!(r4_right.len() >= 256);

        // Row 5: large incompressible chunk in a separate frame so trigram bloom is emitted
        let mut noise = Vec::with_capacity(8192);
        let mut state: u64 = 0xfeed_face_cafe_beef;
        for _ in 0..8192 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            // Keep bytes in '0'..='9' + 'g'..='z' so no accidental match on test needles,
            // yet high enough entropy that stored_len >= 2560!
            let b = b'g' + ((state >> 24) as u8 % 16);
            noise.push(b);
        }
        // Make sure frame 1+ gets >= 2560 B stored by adding a few noise chunks
        let mut r2_slices = Vec::new();
        chunk_payload(row2_bytes.as_bytes(), &mut r2_slices);
        let mut r2_refs = Vec::new();
        for s in r2_slices {
            r2_refs.push((0u16, builder.intern_chunk(s).unwrap()));
        }

        let r3_c0 = builder.intern_chunk(r3_left.as_bytes()).unwrap();
        let r3_c1 = builder.intern_chunk(r3_right.as_bytes()).unwrap();
        let r4_c0 = builder.intern_chunk(r4_left.as_bytes()).unwrap();
        let r4_c1 = builder.intern_chunk(r4_right.as_bytes()).unwrap();

        let mut r5_refs = Vec::new();
        for n in 0..4u8 {
            let mut chunk = noise.clone();
            chunk[0] = b'0' + n;
            r5_refs.push((0u16, builder.intern_chunk(&chunk).unwrap()));
        }

        let mut pack_bytes = Vec::new();
        builder.finish(&mut pack_bytes).unwrap();

        let row_refs = vec![
            PayloadRefRow::Null,
            PayloadRefRow::Inline(b"inline TOKEN_INTERIOR_99 ok".to_vec()),
            PayloadRefRow::Chunks(r2_refs),
            PayloadRefRow::Chunks(vec![(0, r3_c0), (0, r3_c1)]),
            PayloadRefRow::Chunks(vec![(0, r4_c0), (0, r4_c1)]),
            PayloadRefRow::Chunks(r5_refs),
        ];

        let s3 = Arc::new(SimulatedS3Reader::from_bytes(
            pack_bytes,
            SimulatedS3Config::default(),
        ));
        let pack = ChunkPackReader::open(s3).unwrap();

        // 1. Exact interior + inline match
        let bm = search_pack_rows(&pack, &row_refs, 0, b"TOKEN_INTERIOR_99", false).unwrap();
        assert_eq!(bm.iter_ones().collect::<Vec<_>>(), vec![1, 2]);

        // 2. Boundary match (needle <= 64 B spanning r3_c0 || r3_c1)
        let bm = search_pack_rows(&pack, &row_refs, 0, b"CROSS_BOUNDARY_NEEDLE_42", false).unwrap();
        assert_eq!(bm.iter_ones().collect::<Vec<_>>(), vec![3]);

        // Also test case-insensitive boundary match
        let bm = search_pack_rows(&pack, &row_refs, 0, b"cross_boundary_needle_42", true).unwrap();
        assert_eq!(bm.iter_ones().collect::<Vec<_>>(), vec![3]);

        // 3. Absent needle
        let bm = search_pack_rows(&pack, &row_refs, 0, b"DEFINITELY_NOT_PRESENT", false).unwrap();
        assert_eq!(bm.count_ones(), 0);

        // 4. Long needle (> 64 B) spanning r4_c0 || r4_c1
        let bm = search_pack_rows(&pack, &row_refs, 0, long_needle.as_bytes(), false).unwrap();
        assert_eq!(bm.iter_ones().collect::<Vec<_>>(), vec![4]);

        // 5. Unicode ILIKE (non-ASCII UTF-8 'é' and 'Ü' folded to lowercase)
        let bm =
            search_pack_rows(&pack, &row_refs, 0, "cafÉ_über_secret".as_bytes(), true).unwrap();
        assert_eq!(bm.iter_ones().collect::<Vec<_>>(), vec![2]);

        // Exact case-sensitive mismatch for wrong Unicode case
        let bm =
            search_pack_rows(&pack, &row_refs, 0, "cafÉ_über_secret".as_bytes(), false).unwrap();
        assert_eq!(bm.count_ones(), 0);

        // 6. Short needle (< 3 bytes, 0 trigrams) still finds exact matches without bloom pruning
        let bm = search_pack_rows(&pack, &row_refs, 0, b"42", false).unwrap();
        assert_eq!(bm.iter_ones().collect::<Vec<_>>(), vec![3]);

        // 7. Empty needle matches all non-null rows (1..=5, excluding 0=Null)
        let bm = search_pack_rows(&pack, &row_refs, 0, b"", false).unwrap();
        assert_eq!(bm.iter_ones().collect::<Vec<_>>(), vec![1, 2, 3, 4, 5]);

        // 8. Chunk-level search API check
        let reader_slice =
            ChunkPackReader::open(Arc::new(SliceReader::new(Vec::new()))).unwrap_err();
        assert!(matches!(reader_slice, StatamError::Corrupt { .. }));
        let chunk_hits = search_pack_chunks(&pack, b"TOKEN_INTERIOR_99", false).unwrap();
        assert_eq!(chunk_hits, vec![0]);
    }

    #[test]
    fn substring_search_boundary_succeeds_when_both_frames_pruned_by_trigram_bloom() {
        let opts = PackWriteOptions {
            codec: test_codec(),
            zstd_level: 3,
            target_frame_raw_bytes: 12 * 1024,
            enable_trigram_skip: true,
            compress_toc_lz4: true,
        };
        let mut builder = ChunkPackBuilder::new([0x77u8; 16], opts);

        // Two separate frames (each >= 2560 B stored and low bit density so trigram blooms ARE emitted).
        // Left chunk uses only 'a'..'d' + ends with "SPLIT_"; right chunk uses only 'e'..'h' + starts with "HERE!".
        // Needle "SPLIT_HERE!" has trigrams ("lit", "it_", "t_h", "_he", "her") NOT present in either chunk individually!
        let mut state: u64 = 0x9988_7766_5544_3322;
        let mut left = Vec::with_capacity(13 * 1024);
        for _ in 0..(13 * 1024 - 6) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            left.push(b'a' + ((state >> 24) as u8 % 4));
        }
        left.extend_from_slice(b"SPLIT_");

        let mut right = Vec::with_capacity(13 * 1024);
        right.extend_from_slice(b"HERE!");
        for _ in 0..(13 * 1024 - 5) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            right.push(b'm' + ((state >> 24) as u8 % 4));
        }

        let cid_l = builder.intern_chunk(&left).unwrap();
        let cid_r = builder.intern_chunk(&right).unwrap();

        let mut pack_bytes = Vec::new();
        builder.finish(&mut pack_bytes).unwrap();

        let s3 = Arc::new(SimulatedS3Reader::from_bytes(
            pack_bytes,
            SimulatedS3Config::default(),
        ));
        let pack = ChunkPackReader::open(s3).unwrap();
        assert_eq!(pack.toc().frames.len(), 2);
        let blooms = pack.read_trigram_section().unwrap();
        assert!(blooms[0].is_some() && blooms[1].is_some());

        let mut needle_tris = Vec::new();
        extract_trigram_hashes(b"SPLIT_HERE!", false, &mut needle_tris);
        assert!(!blooms[0].as_ref().unwrap().may_contain_hashes(&needle_tris));
        assert!(!blooms[1].as_ref().unwrap().may_contain_hashes(&needle_tris));

        // Even though BOTH frames are pruned by Step A, Step B's EdgeTable finds the exact match!
        let rows = vec![PayloadRefRow::Chunks(vec![(0, cid_l), (0, cid_r)])];
        let bm = search_pack_rows(&pack, &rows, 0, b"SPLIT_HERE!", false).unwrap();
        assert_eq!(bm.iter_ones().collect::<Vec<_>>(), vec![0]);
    }

    #[test]
    fn unicode_ilike_skips_bloom_for_non_ascii_and_clipped_edge_avoids_fffd_false_positive() {
        let opts = PackWriteOptions {
            codec: test_codec(),
            zstd_level: 3,
            target_frame_raw_bytes: 10 * 1024,
            enable_trigram_skip: true,
            compress_toc_lz4: true,
        };
        let mut builder = ChunkPackBuilder::new([0x88u8; 16], opts);

        // Build >= 2560 B compressed frame with low trigram density so bloom IS emitted:
        // Chunk 0 contains "i\u{0307}" (lowercase decomposition of 'İ') and "kelvin_k" (lowercase 'K')
        // inside a high-entropy ASCII alphabet that omits many trigrams.
        let mut state: u64 = 0x1357_2468_9abc_def1;
        let mut c0 = Vec::with_capacity(11 * 1024);
        for _ in 0..10_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            c0.push(b'a' + ((state >> 24) as u8 % 4));
        }
        c0.extend_from_slice("prefix_i\u{0307}_mid_k_suffix".as_bytes());
        let cid0 = builder.intern_chunk(&c0).unwrap();

        // Chunk 1 & Chunk 2: boundary test where a 4-byte emoji '🚀' is clipped right across the
        // 63-byte edge of left_tail AND another '🚀' is clipped at the 63-byte edge of right_head!
        // Left chunk: 200 'x' + '🚀' (4 bytes) + 61 'y' => total 265 B.
        // Last 63 bytes of left_chunk start at byte 202 = 2nd continuation byte of '🚀' ([0x9A, 0x80] || 61*'y')!
        let mut left_clipped = vec![b'x'; 200];
        left_clipped.extend_from_slice("🚀".as_bytes());
        left_clipped.extend_from_slice(&[b'y'; 61]);
        assert!(std::str::from_utf8(&left_clipped).is_ok());

        // Right chunk: 61 'z' + '🚀' (4 bytes) + 200 'w' => total 265 B.
        // First 63 bytes of right_chunk end at byte 63 = 61*'z' || [0xF0, 0x9F] (incomplete trailing '🚀')!
        let mut right_clipped = vec![b'z'; 61];
        right_clipped.extend_from_slice("🚀".as_bytes());
        right_clipped.extend_from_slice(&[b'w'; 200]);
        assert!(std::str::from_utf8(&right_clipped).is_ok());

        // Also a pair that ACTUALLY contains a real U+FFFD at the boundary!
        let mut left_real_fffd = vec![b'p'; 250];
        left_real_fffd.extend_from_slice("\u{FFFD}".as_bytes());
        let right_real_fffd = vec![b'q'; 260];

        let cid_l = builder.intern_chunk(&left_clipped).unwrap();
        let cid_r = builder.intern_chunk(&right_clipped).unwrap();
        let cid_fl = builder.intern_chunk(&left_real_fffd).unwrap();
        let cid_fr = builder.intern_chunk(&right_real_fffd).unwrap();

        let mut pack_bytes = Vec::new();
        builder.finish(&mut pack_bytes).unwrap();
        let pack = ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes))).unwrap();
        let blooms = pack.read_trigram_section().unwrap();
        assert!(blooms[0].is_some(), "frame 0 must emit trigram bloom");

        let rows = vec![
            PayloadRefRow::Chunks(vec![(0, cid0)]),
            PayloadRefRow::Chunks(vec![(0, cid_l), (0, cid_r)]),
            PayloadRefRow::Chunks(vec![(0, cid_fl), (0, cid_fr)]),
        ];

        // Unicode ILIKE with 'İ' (U+0130) -> to_lowercase() == "i\u{0307}" MUST match row 0!
        let bm_i_dot = search_pack_rows(&pack, &rows, 0, "prefix_İ_mid".as_bytes(), true).unwrap();
        assert_eq!(bm_i_dot.iter_ones().collect::<Vec<_>>(), vec![0]);

        // Unicode ILIKE with 'K' (U+212A Kelvin sign) -> to_lowercase() == "k" MUST match row 0!
        let bm_kelvin = search_pack_rows(&pack, &rows, 0, "mid_K_suffix".as_bytes(), true).unwrap();
        assert_eq!(bm_kelvin.iter_ones().collect::<Vec<_>>(), vec![0]);

        // Searching ILIKE for U+FFFD MUST NOT false-positive on row 1's clipped '🚀' edges,
        // while still matching row 2's genuine U+FFFD!
        let bm_fffd = search_pack_rows(&pack, &rows, 0, "\u{FFFD}".as_bytes(), true).unwrap();
        assert_eq!(bm_fffd.iter_ones().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn pigeonhole_boundary_bloom_zero_false_negatives_for_all_splits_5_to_64() {
        // Construct a pack with TS02 header (`edge_tail_bloom` + `edge_head_bloom`) and
        // test EVERY boundary split `1..needle.len()` for needles of length `5..=64`,
        // both case-sensitive (`LIKE`) and ASCII case-insensitive (`ILIKE`), ensuring
        // Pigeonhole boundary bloom NEVER causes a false negative AND skips EdgeTable when absent.
        let opts = PackWriteOptions {
            codec: test_codec(),
            zstd_level: 3,
            target_frame_raw_bytes: 128 * 1024,
            enable_trigram_skip: true,
            compress_toc_lz4: true,
        };
        let mut builder = ChunkPackBuilder::new([0x42u8; 16], opts);

        // Emit one low-density frame >= 1024 B compressed so TS02 header is always written.
        let mut state: u64 = 0x55aa_1122_3344_5566;
        let mut filler = Vec::with_capacity(12 * 1024);
        for _ in 0..(12 * 1024) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            filler.push(b'a' + ((state >> 24) as u8 % 4));
        }
        let filler_cid = builder.intern_chunk(&filler).unwrap();

        // Master 64-byte needle mixing uppercase, lowercase, digits, and symbols ('E'..'Z', '0'..'9', '_').
        let full_needle: Vec<u8> = (0..64)
            .map(|i| match i % 3 {
                0 => b'E' + (i as u8 % 12),
                1 => b'0' + (i as u8 % 10),
                _ => b'_',
            })
            .collect();

        // Build one representative split pair for every (len in 5..=64, split in 1..len).
        // Group splits into a few tests across chunk pairs by storing each (len, split) pair
        // in its own chunk pair (using unique length-prefix padding in `'a'..'d'` so chunks are >= 256 B).
        let mut cases = Vec::new();
        for len in 5..=64usize {
            let needle = &full_needle[..len];
            for split in 1..len {
                let mut left = vec![b'a'; 260];
                // Encode unique marker in `'a'..'d'` so chunks don't dedup into identical IDs.
                let tag = ((len * 64 + split) as u32).to_le_bytes();
                for (j, b) in tag.iter().enumerate() {
                    left[j * 2] = b'a' + (b & 3);
                    left[j * 2 + 1] = b'a' + ((b >> 2) & 3);
                }
                left.extend_from_slice(&needle[..split]);

                let mut right = needle[split..].to_vec();
                right.extend_from_slice(&vec![b'b'; 260]);
                for (j, b) in tag.iter().enumerate() {
                    let pos = right.len() - 16 + j * 2;
                    right[pos] = b'a' + ((b >> 4) & 3);
                    right[pos + 1] = b'a' + ((b >> 6) & 3);
                }

                let lc = builder.intern_chunk(&left).unwrap();
                let rc = builder.intern_chunk(&right).unwrap();
                cases.push((len, split, lc, rc));
            }
        }

        // Also insert chunks whose edge blooms contain single-trigram collisions (`nev`, `zqx` in tail;
        // `987`, `dle` in head) to verify half-needle pigeonhole still rejects full absent needles.
        let mut edge_coll1 = vec![b'a'; 256];
        edge_coll1[..3].copy_from_slice(b"dle");
        edge_coll1[253..256].copy_from_slice(b"nev");
        builder.intern_chunk(&edge_coll1).unwrap();
        let mut edge_coll2 = vec![b'b'; 256];
        edge_coll2[..3].copy_from_slice(b"987");
        edge_coll2[253..256].copy_from_slice(b"zqx");
        builder.intern_chunk(&edge_coll2).unwrap();

        let mut pack_bytes = Vec::new();
        builder.finish(&mut pack_bytes).unwrap();

        let s3 = Arc::new(SimulatedS3Reader::from_bytes(
            pack_bytes,
            SimulatedS3Config::default(),
        ));
        let pack = ChunkPackReader::open(s3.clone()).unwrap();
        let edge_blooms = pack.read_edge_trigram_blooms().unwrap();
        assert!(edge_blooms.is_some(), "TS02 edge blooms must be present");

        // Verify every single (len in 5..=64, split in 1..len) is NOT rejected by Pigeonhole
        // and matches via `search_pack_rows` for both LIKE and ASCII-lowercased ILIKE!
        for &(len, _split, lc, rc) in &cases {
            let needle = &full_needle[..len];
            assert!(
                !pack_rejects_boundary_pigeonhole(edge_blooms.as_ref(), needle, false),
                "false negative on LIKE pigeonhole check for len={len}"
            );
            let lower_needle: Vec<u8> = needle.iter().map(u8::to_ascii_lowercase).collect();
            assert!(
                !pack_rejects_boundary_pigeonhole(edge_blooms.as_ref(), &lower_needle, true),
                "false negative on ILIKE pigeonhole check for len={len}"
            );

            let rows = vec![
                PayloadRefRow::Chunks(vec![(0, filler_cid)]),
                PayloadRefRow::Chunks(vec![(0, lc), (0, rc)]),
            ];
            let bm_like = search_pack_rows(&pack, &rows, 0, needle, false).unwrap();
            assert_eq!(bm_like.iter_ones().collect::<Vec<_>>(), vec![1]);

            let bm_ilike = search_pack_rows(&pack, &rows, 0, &lower_needle, true).unwrap();
            assert_eq!(bm_ilike.iter_ones().collect::<Vec<_>>(), vec![1]);
        }

        // Verify absent needles are rejected even when single edge trigrams (`nev`, `dle`, `987`, `zqx`) collide!
        let fresh_pack = ChunkPackReader::open(s3.clone()).unwrap();
        s3.reset_counters();
        let absent = b"XYZ_NEVER_IN_EDGES_999";
        let ts = fresh_pack.trigram_section_cached().unwrap();
        assert!(pack_rejects_boundary_pigeonhole(
            ts.edge_blooms.as_ref(),
            absent,
            false
        ));
        assert!(pack_rejects_boundary_pigeonhole(
            ts.edge_blooms.as_ref(),
            b"never_present_xyz987",
            false
        ));
        assert!(pack_rejects_boundary_pigeonhole(
            ts.edge_blooms.as_ref(),
            b"zqxj_rare_needle",
            false
        ));
        let rows = vec![PayloadRefRow::Chunks(vec![
            (0, cases[0].2),
            (0, cases[0].3),
        ])];
        let bm_abs = search_pack_rows(&fresh_pack, &rows, 0, absent, false).unwrap();
        assert_eq!(bm_abs.count_ones(), 0);
        // Only the TS02 trigram section was read (never EdgeTable or data frames)!
        let counters_after = s3.counters();
        assert!(counters_after.bytes_requested <= fresh_pack.postscript().trigram_len as u64);
    }
}
