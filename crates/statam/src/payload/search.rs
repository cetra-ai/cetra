//! Exact substring search (`LIKE` / `ILIKE`) over inline payloads and `.stpk` chunk packs.

use std::collections::{BTreeSet, HashMap, HashSet};

use cetra_column::Bitmap;
use memchr::memmem;

use crate::StatamError;
use crate::format::MAX_ROW_PAYLOAD_BYTES;
use crate::payload::PayloadRefRow;
use crate::payload::pack::ChunkPackReader;
use crate::simd::trigram::{ascii_lowercase_into, extract_trigram_hashes};

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
    let mut results = Bitmap::new_zeroed(row_refs.len());

    if needle.is_empty() {
        for (idx, row) in row_refs.iter().enumerate() {
            if !matches!(row, PayloadRefRow::Null) {
                let _ = results.set(idx, true);
            }
        }
        return Ok(results);
    }

    let mut matcher = NeedleMatcher::new(needle, case_insensitive);
    let mut chunk_rows = Vec::new();
    let mut needed_chunks = BTreeSet::new();
    let pack_chunk_count = pack.toc().chunks.len();

    for (row_idx, row) in row_refs.iter().enumerate() {
        match row {
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
    }

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
    // Step A: Interior matches via trigram bloom pruning + single-pass unique chunk scan.
    let all_needed: Vec<u32> = needed_chunks.iter().copied().collect();
    let surviving_chunks =
        filter_candidates_by_trigram(pack, &all_needed, needle, case_insensitive)?;

    let mut matched_chunks = HashSet::new();
    if !surviving_chunks.is_empty() {
        let fetched = pack.read_chunks(&surviving_chunks)?;
        for cid in surviving_chunks {
            let bytes = fetched.get(&cid).ok_or(StatamError::Corrupt {
                reason: "missing fetched chunk during short-needle search",
            })?;
            if matcher.matches(bytes) {
                matched_chunks.insert(cid);
            }
        }
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
    let mut distinct_pairs = BTreeSet::new();
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
        combined.extend_from_slice(left_tail);
        combined.extend_from_slice(right_head);
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

    let mut hashes = Vec::new();
    extract_trigram_hashes(needle, case_insensitive, &mut hashes);
    if hashes.is_empty() {
        return Ok(chunk_ids.to_vec());
    }

    let blooms = pack.read_trigram_section()?;
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
}
