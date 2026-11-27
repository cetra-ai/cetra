//! Exact substring search (`LIKE` / `ILIKE`) over inline payloads and `.stpk` chunk packs.

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasherDefault;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use cetra_column::Bitmap;
use memchr::memmem;

use crate::StatamError;
use crate::format::{ChunkLoc, MAX_ROW_PAYLOAD_BYTES};
use crate::payload::pack::ChunkPackReader;
use crate::payload::{
    FLAT_REF_CHUNKS, FLAT_REF_INLINE, FLAT_REF_NULL, FlatPayloadRefs, PayloadRefRow,
};
use crate::simd::trigram::{
    TrigramBloom, ascii_lowercase_append, ascii_lowercase_into, extract_trigram_hashes,
};

/// Maximum needle byte length guaranteed to span at most two adjacent `>= 256 B` chunks
/// and lie completely inside `tail_63(left) || head_63(right)` when crossing a boundary.
const MAX_EDGE_NEEDLE_LEN: usize = 64;
/// Maximum total `.stpk` chunk bytes retained from Phase-1 substring scans for Phase-2 reuse (`64 MiB`).
pub(crate) const PHASE1_RETAINED_CHUNKS_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Thread-safe, strictly bounded cache of `.stpk` chunk bytes retained during Phase-1 substring
/// evaluation so Phase-2 materialization can skip re-reading matched chunks from `.stpk`.
///
/// Peak retained + in-flight reserved bytes across all concurrent frames, mini-blocks, and
/// row-groups sharing this instance can never exceed `max_bytes` (default `64 MiB`).
pub(crate) struct RetainedPackChunks {
    #[cfg(test)]
    max_bytes: usize,
    remaining: AtomicUsize,
    chunks: Mutex<HashMap<([u8; 16], u32), Bytes>>,
}

impl Default for RetainedPackChunks {
    fn default() -> Self {
        Self::new(PHASE1_RETAINED_CHUNKS_MAX_BYTES)
    }
}

impl RetainedPackChunks {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            #[cfg(test)]
            max_bytes,
            remaining: AtomicUsize::new(max_bytes),
            chunks: Mutex::new(HashMap::new()),
        }
    }

    #[inline]
    pub(crate) fn remaining_bytes(&self) -> usize {
        self.remaining.load(Ordering::Acquire)
    }

    #[inline]
    #[cfg(test)]
    pub(crate) fn used_bytes(&self) -> usize {
        self.max_bytes.saturating_sub(self.remaining_bytes())
    }

    /// Atomically reserves `len` bytes from the remaining retention budget.
    #[inline]
    pub(crate) fn try_reserve(&self, len: usize) -> bool {
        self.remaining
            .try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                cur.checked_sub(len)
            })
            .is_ok()
    }

    /// Releases `len` previously reserved bytes back to the retention budget.
    #[inline]
    pub(crate) fn release(&self, len: usize) {
        self.remaining.fetch_add(len, Ordering::AcqRel);
    }

    /// Inserts `(pack_id, cid) -> data` if `data.len()` fits within the remaining budget.
    ///
    /// If `(pack_id, cid)` is already retained, drops `data` and refunds its reservation.
    pub(crate) fn insert_if_fits(&self, pack_id: [u8; 16], cid: u32, data: Bytes) -> bool {
        let len = data.len();
        if !self.try_reserve(len) {
            return false;
        }
        let mut guard = self.chunks.lock().unwrap_or_else(|p| p.into_inner());
        let key = (pack_id, cid);
        if let std::collections::hash_map::Entry::Vacant(e) = guard.entry(key) {
            e.insert(data);
            true
        } else {
            drop(guard);
            drop(data);
            self.release(len);
            true
        }
    }

    /// Inserts chunks whose byte lengths were already reserved via [`Self::try_reserve`],
    /// refunding the byte budget for any chunk keys that were already retained.
    fn insert_reserved_batch(&self, pack_id: [u8; 16], items: Vec<(u32, Bytes)>) {
        if items.is_empty() {
            return;
        }
        let mut refund = 0usize;
        let mut guard = self.chunks.lock().unwrap_or_else(|p| p.into_inner());
        for (cid, data) in items {
            let len = data.len();
            if let std::collections::hash_map::Entry::Vacant(e) = guard.entry((pack_id, cid)) {
                e.insert(data);
            } else {
                drop(data);
                refund = refund.saturating_add(len);
            }
        }
        drop(guard);
        if refund > 0 {
            self.release(refund);
        }
    }

    pub(crate) fn into_map(self) -> HashMap<([u8; 16], u32), Bytes> {
        self.chunks.into_inner().unwrap_or_else(|p| p.into_inner())
    }
}

/// RAII guard for per-frame reserved chunk copies that automatically refunds its atomic
/// budget if the frame callback exits early with an error before committing.
struct ReservedFrameChunks<'a> {
    retained: &'a RetainedPackChunks,
    items: Vec<(u32, Bytes)>,
    reserved_bytes: usize,
}

impl<'a> ReservedFrameChunks<'a> {
    fn new(retained: &'a RetainedPackChunks) -> Self {
        Self {
            retained,
            items: Vec::new(),
            reserved_bytes: 0,
        }
    }

    fn try_push(&mut self, cid: u32, slice: &[u8]) -> bool {
        let len = slice.len();
        if !self.retained.try_reserve(len) {
            return false;
        }
        self.reserved_bytes = self.reserved_bytes.saturating_add(len);
        self.items.push((cid, Bytes::copy_from_slice(slice)));
        true
    }

    fn commit(mut self, pack_id: [u8; 16]) {
        let items = std::mem::take(&mut self.items);
        self.reserved_bytes = 0;
        self.retained.insert_reserved_batch(pack_id, items);
    }
}

impl Drop for ReservedFrameChunks<'_> {
    fn drop(&mut self) {
        if self.reserved_bytes > 0 {
            self.items.clear();
            self.retained.release(self.reserved_bytes);
            self.reserved_bytes = 0;
        }
    }
}

/// Compact O(total_chunk_refs) index of chunks sharing a `(row, frame_id)` group so matched
/// multi-chunk rows can retain their same-frame sibling chunks without O(K^2) expansion.
struct SameFrameSiblings {
    groups: Vec<Vec<u32>>,
    cid_to_groups: HashMap<u32, Vec<usize>>,
}

impl SameFrameSiblings {
    fn build<S: PayloadRefSource>(
        row_refs: &S,
        chunk_rows: &[usize],
        pack_idx: u16,
        toc_chunks: &[ChunkLoc],
    ) -> Self {
        let mut groups: Vec<Vec<u32>> = Vec::new();
        let mut cid_to_groups: HashMap<u32, Vec<usize>> = HashMap::new();
        let mut by_frame: Vec<(u32, u32)> = Vec::new();

        for &row_idx in chunk_rows {
            by_frame.clear();
            let _ = row_refs.for_each_chunk(row_idx, &mut |p, cid| {
                if p == pack_idx {
                    by_frame.push((toc_chunks[cid as usize].frame_id, cid));
                }
                Ok(())
            });
            if by_frame.len() <= 1 {
                continue;
            }
            by_frame.sort_unstable();
            by_frame.dedup();
            let mut i = 0usize;
            while i < by_frame.len() {
                let fid = by_frame[i].0;
                let mut j = i + 1;
                while j < by_frame.len() && by_frame[j].0 == fid {
                    j += 1;
                }
                if j - i > 1 {
                    let group_idx = groups.len();
                    let mut group = Vec::with_capacity(j - i);
                    for &(_, cid) in &by_frame[i..j] {
                        group.push(cid);
                        cid_to_groups.entry(cid).or_default().push(group_idx);
                    }
                    groups.push(group);
                }
                i = j;
            }
        }

        Self {
            groups,
            cid_to_groups,
        }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    #[inline]
    fn group_indices(&self, cid: u32) -> Option<&[usize]> {
        self.cid_to_groups.get(&cid).map(Vec::as_slice)
    }

    #[inline]
    fn group(&self, group_idx: usize) -> &[u32] {
        &self.groups[group_idx]
    }
}

/// Context-free Unicode lowercasing for `ILIKE` matching.
///
/// Lowers each Unicode scalar independently (`char::to_lowercase`) and folds Greek Final Sigma
/// `'ς'` (`U+03C2`) to `'σ'` (`U+03C3`), guaranteeing strict homomorphism
/// `lower(A || B) == lower(A) || lower(B)` across arbitrary UTF-8 chunk boundaries while keeping
/// `"ΟΔΥΣΣΕΥΣ" ILIKE '%ς'` exact.
pub(crate) fn unicode_lowercase_context_free_into(s: &str, out: &mut String) {
    out.clear();
    out.reserve(s.len());
    for c in s.chars() {
        if c == '\u{03C2}' {
            out.push('\u{03C3}');
        } else {
            for lc in c.to_lowercase() {
                if lc == '\u{03C2}' {
                    out.push('\u{03C3}');
                } else {
                    out.push(lc);
                }
            }
        }
    }
}

#[inline]
fn ascii_prefix_len(bytes: &[u8]) -> usize {
    const NON_ASCII_MASK: u64 = 0x8080_8080_8080_8080;
    let (chunks, rem) = bytes.as_chunks::<8>();
    for (idx, &chunk) in chunks.iter().enumerate() {
        if (u64::from_ne_bytes(chunk) & NON_ASCII_MASK) != 0 {
            let base = idx * 8;
            for (j, &b) in chunk.iter().enumerate() {
                if b >= 0x80 {
                    return base + j;
                }
            }
        }
    }
    let base = chunks.len() * 8;
    for (j, &b) in rem.iter().enumerate() {
        if b >= 0x80 {
            return base + j;
        }
    }
    bytes.len()
}

/// Extracts an exact ASCII pre-filter anchor from a context-free Unicode-lowercased `ILIKE` needle.
///
/// Under Rust's `char::to_lowercase()`, the only non-ASCII scalar values (`c >= '\u{0080}'`) whose
/// lowercase expansion emits any ASCII byte (`0x00..=0x7F`) are:
/// - `U+0130` (`'İ' -> "i\u{0307}"`), where `'i'` is always immediately followed by non-ASCII `0xCC`,
/// - `U+212A` (`'K' -> "k"`).
///
/// Therefore, any contiguous ASCII slice inside `uni_lower` that contains no `b'k'` and does not end
/// with a trailing `b'i'` at the end of its maximal ASCII run can only arise from matching ASCII
/// bytes in the haystack (under `ascii_lowercase`). Requiring `memmem::find(needle_raw_lower, cand)`
/// additionally guarantees zero false negatives on the invalid-UTF-8 raw-ASCII fallback path.
pub(crate) fn extract_unicode_ilike_ascii_anchor(
    uni_lower: &[u8],
    needle_raw_lower: &[u8],
) -> Option<Vec<u8>> {
    let mut best: Option<&[u8]> = None;
    let mut i = 0usize;
    while i < uni_lower.len() {
        if uni_lower[i] >= 0x80 {
            i += 1;
            continue;
        }
        let run_start = i;
        while i < uni_lower.len() && uni_lower[i] < 0x80 {
            i += 1;
        }
        let mut run = &uni_lower[run_start..i];
        // A trailing 'i' at the end of a maximal ASCII run may be followed by non-ASCII `0xCC`
        // (from `U+0130` -> "i\u{0307}"); an 'i' followed by another ASCII byte inside the run
        // cannot come from `U+0130`.
        if run.last() == Some(&b'i') {
            run = &run[..run.len() - 1];
        }
        for cand in run.split(|&b| b == b'k') {
            if cand.len() >= 2
                && best.is_none_or(|b| cand.len() > b.len())
                && memmem::find(needle_raw_lower, cand).is_some()
            {
                best = Some(cand);
            }
        }
    }
    best.map(<[u8]>::to_vec)
}

/// Fast SIMD-accelerated context-free Unicode lowercasing into a UTF-8 byte buffer.
///
/// Scans contiguous ASCII runs and lowers them directly via SIMD `ascii_lowercase_append`
/// without invoking `char::to_lowercase`, copies caseless 3-byte CJK/Hangul runs directly,
/// and falls back to scalar `char::to_lowercase` (with Greek Final Sigma `'ς'` -> `'σ'` folding)
/// only on other non-ASCII codepoints.
///
/// Output is bit-exact with [`unicode_lowercase_context_free_into`] for every UTF-8 input.
pub(crate) fn unicode_lowercase_context_free_into_bytes(s: &str, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    let mut enc = [0u8; 4];
    while i < bytes.len() {
        if bytes[i] < 0x80 {
            let ascii_len = ascii_prefix_len(&bytes[i..]);
            ascii_lowercase_append(&bytes[i..i + ascii_len], out);
            i += ascii_len;
            continue;
        }
        let Some(c) = s[i..].chars().next() else {
            break;
        };
        // CJK punctuation/Kana/CJK Unified Ideographs (`U+3000..=U+9FFF`) and Hangul Syllables
        // (`U+AC00..=U+D7AF`) are 100% caseless and 3-byte UTF-8; copy contiguous runs directly.
        if matches!(c, '\u{3000}'..='\u{9FFF}' | '\u{AC00}'..='\u{D7AF}') {
            let run_start = i;
            i += 3;
            while i < bytes.len() {
                let b = bytes[i];
                if (0xE3..=0xE9).contains(&b) || b == 0xEB || b == 0xEC {
                    i += 3;
                } else if b == 0xEA || b == 0xED {
                    if let Some(nc) = s[i..].chars().next()
                        && matches!(nc, '\u{AC00}'..='\u{D7AF}')
                    {
                        i += 3;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            out.extend_from_slice(&bytes[run_start..i]);
            continue;
        }
        if c == '\u{03C2}' {
            out.extend_from_slice('\u{03C3}'.encode_utf8(&mut enc).as_bytes());
        } else {
            for lc in c.to_lowercase() {
                let mapped = if lc == '\u{03C2}' { '\u{03C3}' } else { lc };
                out.extend_from_slice(mapped.encode_utf8(&mut enc).as_bytes());
            }
        }
        i += c.len_utf8();
    }
}

enum NeedleMatcher {
    Exact(memmem::Finder<'static>),
    AsciiIlike {
        finder: memmem::Finder<'static>,
        scratch: Vec<u8>,
    },
    UnicodeIlike {
        finder: memmem::Finder<'static>,
        ascii_anchor: Option<Box<memmem::Finder<'static>>>,
        needle_has_non_ascii: bool,
        needle_raw_lower: Vec<u8>,
        scratch: Vec<u8>,
        uni_scratch: Vec<u8>,
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
                let mut uni_lower = Vec::new();
                unicode_lowercase_context_free_into_bytes(s, &mut uni_lower);
                let needle_has_non_ascii = !uni_lower.is_ascii();
                let ascii_anchor = extract_unicode_ilike_ascii_anchor(&uni_lower, &ascii_lowered)
                    .map(|anchor| Box::new(memmem::Finder::new(&anchor).into_owned()));
                Self::UnicodeIlike {
                    finder: memmem::Finder::new(&uni_lower).into_owned(),
                    ascii_anchor,
                    needle_has_non_ascii,
                    needle_raw_lower: ascii_lowered,
                    scratch: Vec::new(),
                    uni_scratch: Vec::new(),
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
                ascii_anchor,
                needle_has_non_ascii,
                needle_raw_lower,
                scratch,
                uni_scratch,
            } => {
                if *needle_has_non_ascii && haystack.is_ascii() {
                    return false;
                }
                if let Some(anchor) = ascii_anchor {
                    ascii_lowercase_into(haystack, scratch);
                    if anchor.find(scratch).is_none() {
                        return false;
                    }
                }
                if let Ok(s) = std::str::from_utf8(haystack) {
                    unicode_lowercase_context_free_into_bytes(s, uni_scratch);
                    finder.find(uni_scratch).is_some()
                } else {
                    // Fallback for invalid UTF-8 slices (e.g. clipped boundary edge):
                    // try lossy UTF-8 Unicode lowercase AND raw ASCII lowercase.
                    let lossy = String::from_utf8_lossy(haystack);
                    unicode_lowercase_context_free_into_bytes(&lossy, uni_scratch);
                    if finder.find(uni_scratch).is_some() {
                        return true;
                    }
                    if ascii_anchor.is_none() {
                        ascii_lowercase_into(haystack, scratch);
                    }
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
    search_pack_rows_impl(
        pack,
        &row_refs,
        None,
        pack_idx,
        needle,
        case_insensitive,
        1,
        None,
    )
}

/// Exact row-level substring search restricted to rows active in `active_mask`,
/// using up to `read_threads` workers for `.stpk` frame decompression and substring scanning.
#[cfg(test)]
pub(crate) fn search_pack_rows_masked_with_threads(
    pack: &ChunkPackReader,
    row_refs: &[PayloadRefRow],
    active_mask: &Bitmap,
    pack_idx: u16,
    needle: &[u8],
    case_insensitive: bool,
    read_threads: usize,
) -> Result<Bitmap, StatamError> {
    search_pack_rows_impl(
        pack,
        &row_refs,
        Some(active_mask),
        pack_idx,
        needle,
        case_insensitive,
        read_threads,
        None,
    )
}

/// Zero-allocation exact row-level substring search over [`FlatPayloadRefs`] restricted to
/// rows active in `active_mask`, optionally retaining matched chunks (and same-frame siblings)
/// into `retain` within its atomic byte budget.
#[allow(clippy::too_many_arguments)]
pub(crate) fn search_pack_rows_flat_masked_with_threads(
    pack: &ChunkPackReader,
    row_refs: &FlatPayloadRefs,
    active_mask: &Bitmap,
    pack_idx: u16,
    needle: &[u8],
    case_insensitive: bool,
    read_threads: usize,
    retain: Option<([u8; 16], &RetainedPackChunks)>,
) -> Result<Bitmap, StatamError> {
    search_pack_rows_impl(
        pack,
        &row_refs,
        Some(active_mask),
        pack_idx,
        needle,
        case_insensitive,
        read_threads,
        retain,
    )
}

/// Zero-duplication cross-mini-block exact row-level substring search over multiple
/// [`FlatPayloadRefs`] slices in a single `.stpk` pack read wave.
#[allow(clippy::too_many_arguments)]
pub(crate) fn search_pack_rows_flat_multi_masked_with_threads(
    pack: &ChunkPackReader,
    mb_refs: &[&FlatPayloadRefs],
    mb_masks: &[&Bitmap],
    pack_idx: u16,
    needle: &[u8],
    case_insensitive: bool,
    read_threads: usize,
    retain: Option<([u8; 16], &RetainedPackChunks)>,
) -> Result<Vec<Bitmap>, StatamError> {
    if mb_refs.len() != mb_masks.len() {
        return Err(StatamError::Corrupt {
            reason: "multi-mb payload search refs/masks length mismatch",
        });
    }
    if mb_refs.is_empty() {
        return Ok(Vec::new());
    }
    if mb_refs.len() == 1 {
        let bm = search_pack_rows_flat_masked_with_threads(
            pack,
            mb_refs[0],
            mb_masks[0],
            pack_idx,
            needle,
            case_insensitive,
            read_threads,
            retain,
        )?;
        return Ok(vec![bm]);
    }

    let mut offsets = Vec::with_capacity(mb_refs.len() + 1);
    let mut total_rows = 0usize;
    offsets.push(0);
    for refs in mb_refs {
        total_rows = total_rows
            .checked_add(refs.len())
            .ok_or(StatamError::BoundsExceeded {
                reason: "multi-mb payload search row count overflow",
            })?;
        offsets.push(total_rows);
    }

    let mut combined_mask = Bitmap::new_zeroed(total_rows);
    for (m, mask) in mb_masks.iter().enumerate() {
        let base = offsets[m];
        let mb_len = mb_refs[m].len();
        for r in mask.iter_ones() {
            if r >= mb_len {
                break;
            }
            let _ = combined_mask.set(base + r, true);
        }
    }

    let view = MultiMbFlatPayloadRefs {
        mbs: mb_refs,
        offsets,
    };
    let combined = search_pack_rows_impl(
        pack,
        &view,
        Some(&combined_mask),
        pack_idx,
        needle,
        case_insensitive,
        read_threads,
        retain,
    )?;

    let mut out: Vec<Bitmap> = mb_refs
        .iter()
        .map(|refs| Bitmap::new_zeroed(refs.len()))
        .collect();
    for global_row in combined.iter_ones() {
        let (m, local_row) = view.locate(global_row);
        let _ = out[m].set(local_row, true);
    }
    Ok(out)
}

struct MultiMbFlatPayloadRefs<'a> {
    mbs: &'a [&'a FlatPayloadRefs],
    offsets: Vec<usize>,
}

impl MultiMbFlatPayloadRefs<'_> {
    #[inline]
    fn locate(&self, row: usize) -> (usize, usize) {
        let mb_pos = self.offsets.partition_point(|&off| off <= row) - 1;
        (mb_pos, row - self.offsets[mb_pos])
    }
}

impl PayloadRefSource for MultiMbFlatPayloadRefs<'_> {
    #[inline]
    fn row_count(&self) -> usize {
        self.offsets.last().copied().unwrap_or(0)
    }

    #[inline]
    fn is_null(&self, row: usize) -> bool {
        let (m, r) = self.locate(row);
        self.mbs[m].row_kind(r) == FLAT_REF_NULL
    }

    #[inline]
    fn inline_bytes(&self, row: usize) -> Option<&[u8]> {
        let (m, r) = self.locate(row);
        (self.mbs[m].row_kind(r) == FLAT_REF_INLINE).then(|| self.mbs[m].inline_bytes(r))
    }

    #[inline]
    fn for_each_chunk(
        &self,
        row: usize,
        f: &mut impl FnMut(u16, u32) -> Result<(), StatamError>,
    ) -> Result<(), StatamError> {
        let (m, r) = self.locate(row);
        (&self.mbs[m]).for_each_chunk(r, f)
    }

    #[inline]
    fn any_chunk(&self, row: usize, f: impl FnMut(u16, u32) -> bool) -> bool {
        let (m, r) = self.locate(row);
        (&self.mbs[m]).any_chunk(r, f)
    }

    #[inline]
    fn for_each_adjacent_pair_in_pack(
        &self,
        row: usize,
        pack_idx: u16,
        f: &mut impl FnMut(u32, u32),
    ) {
        let (m, r) = self.locate(row);
        (&self.mbs[m]).for_each_adjacent_pair_in_pack(r, pack_idx, f);
    }

    #[inline]
    fn any_adjacent_pair_in_pack(
        &self,
        row: usize,
        pack_idx: u16,
        f: impl FnMut(u32, u32) -> bool,
    ) -> bool {
        let (m, r) = self.locate(row);
        (&self.mbs[m]).any_adjacent_pair_in_pack(r, pack_idx, f)
    }

    #[inline]
    fn seq_key(&self, row: usize) -> Option<SeqKey<'_>> {
        let (m, r) = self.locate(row);
        (&self.mbs[m]).seq_key(r)
    }
}

#[derive(PartialEq, Eq, Hash)]
enum SeqKey<'a> {
    Interleaved(&'a [(u16, u32)]),
    Split(&'a [u16], &'a [u32]),
}

trait PayloadRefSource {
    fn row_count(&self) -> usize;
    fn is_null(&self, row: usize) -> bool;
    fn inline_bytes(&self, row: usize) -> Option<&[u8]>;
    fn for_each_chunk(
        &self,
        row: usize,
        f: &mut impl FnMut(u16, u32) -> Result<(), StatamError>,
    ) -> Result<(), StatamError>;
    fn any_chunk(&self, row: usize, f: impl FnMut(u16, u32) -> bool) -> bool;
    fn for_each_adjacent_pair_in_pack(
        &self,
        row: usize,
        pack_idx: u16,
        f: &mut impl FnMut(u32, u32),
    );
    fn any_adjacent_pair_in_pack(
        &self,
        row: usize,
        pack_idx: u16,
        f: impl FnMut(u32, u32) -> bool,
    ) -> bool;
    fn seq_key(&self, row: usize) -> Option<SeqKey<'_>>;
}

impl PayloadRefSource for &[PayloadRefRow] {
    #[inline]
    fn row_count(&self) -> usize {
        self.len()
    }

    #[inline]
    fn is_null(&self, row: usize) -> bool {
        matches!(self[row], PayloadRefRow::Null)
    }

    #[inline]
    fn inline_bytes(&self, row: usize) -> Option<&[u8]> {
        match &self[row] {
            PayloadRefRow::Inline(bytes) => Some(bytes),
            _ => None,
        }
    }

    #[inline]
    fn for_each_chunk(
        &self,
        row: usize,
        f: &mut impl FnMut(u16, u32) -> Result<(), StatamError>,
    ) -> Result<(), StatamError> {
        if let PayloadRefRow::Chunks(refs) = &self[row] {
            for &(p, cid) in refs {
                f(p, cid)?;
            }
        }
        Ok(())
    }

    #[inline]
    fn any_chunk(&self, row: usize, mut f: impl FnMut(u16, u32) -> bool) -> bool {
        if let PayloadRefRow::Chunks(refs) = &self[row] {
            refs.iter().any(|&(p, cid)| f(p, cid))
        } else {
            false
        }
    }

    #[inline]
    fn for_each_adjacent_pair_in_pack(
        &self,
        row: usize,
        pack_idx: u16,
        f: &mut impl FnMut(u32, u32),
    ) {
        if let PayloadRefRow::Chunks(refs) = &self[row]
            && refs.len() > 1
        {
            for w in refs.windows(2) {
                if w[0].0 == pack_idx && w[1].0 == pack_idx {
                    f(w[0].1, w[1].1);
                }
            }
        }
    }

    #[inline]
    fn any_adjacent_pair_in_pack(
        &self,
        row: usize,
        pack_idx: u16,
        mut f: impl FnMut(u32, u32) -> bool,
    ) -> bool {
        if let PayloadRefRow::Chunks(refs) = &self[row]
            && refs.len() > 1
        {
            refs.windows(2)
                .any(|w| w[0].0 == pack_idx && w[1].0 == pack_idx && f(w[0].1, w[1].1))
        } else {
            false
        }
    }

    #[inline]
    fn seq_key(&self, row: usize) -> Option<SeqKey<'_>> {
        match &self[row] {
            PayloadRefRow::Chunks(refs) => Some(SeqKey::Interleaved(refs.as_slice())),
            _ => None,
        }
    }
}

impl PayloadRefSource for &FlatPayloadRefs {
    #[inline]
    fn row_count(&self) -> usize {
        self.len()
    }

    #[inline]
    fn is_null(&self, row: usize) -> bool {
        self.row_kind(row) == FLAT_REF_NULL
    }

    #[inline]
    fn inline_bytes(&self, row: usize) -> Option<&[u8]> {
        (self.row_kind(row) == FLAT_REF_INLINE).then(|| FlatPayloadRefs::inline_bytes(self, row))
    }

    #[inline]
    fn for_each_chunk(
        &self,
        row: usize,
        f: &mut impl FnMut(u16, u32) -> Result<(), StatamError>,
    ) -> Result<(), StatamError> {
        if self.row_kind(row) == FLAT_REF_CHUNKS {
            let (ps, cs) = self.chunk_refs(row);
            for (&p, &cid) in ps.iter().zip(cs.iter()) {
                f(p, cid)?;
            }
        }
        Ok(())
    }

    #[inline]
    fn any_chunk(&self, row: usize, mut f: impl FnMut(u16, u32) -> bool) -> bool {
        if self.row_kind(row) == FLAT_REF_CHUNKS {
            let (ps, cs) = self.chunk_refs(row);
            ps.iter().zip(cs.iter()).any(|(&p, &cid)| f(p, cid))
        } else {
            false
        }
    }

    #[inline]
    fn for_each_adjacent_pair_in_pack(
        &self,
        row: usize,
        pack_idx: u16,
        f: &mut impl FnMut(u32, u32),
    ) {
        if self.row_kind(row) == FLAT_REF_CHUNKS {
            let (ps, cs) = self.chunk_refs(row);
            if ps.len() > 1 {
                for i in 0..ps.len() - 1 {
                    if ps[i] == pack_idx && ps[i + 1] == pack_idx {
                        f(cs[i], cs[i + 1]);
                    }
                }
            }
        }
    }

    #[inline]
    fn any_adjacent_pair_in_pack(
        &self,
        row: usize,
        pack_idx: u16,
        mut f: impl FnMut(u32, u32) -> bool,
    ) -> bool {
        if self.row_kind(row) == FLAT_REF_CHUNKS {
            let (ps, cs) = self.chunk_refs(row);
            if ps.len() > 1 {
                for i in 0..ps.len() - 1 {
                    if ps[i] == pack_idx && ps[i + 1] == pack_idx && f(cs[i], cs[i + 1]) {
                        return true;
                    }
                }
            }
        }
        false
    }

    #[inline]
    fn seq_key(&self, row: usize) -> Option<SeqKey<'_>> {
        if self.row_kind(row) == FLAT_REF_CHUNKS {
            let (ps, cs) = self.chunk_refs(row);
            Some(SeqKey::Split(ps, cs))
        } else {
            None
        }
    }
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

#[allow(clippy::too_many_arguments)]
fn search_pack_rows_impl<S: PayloadRefSource>(
    pack: &ChunkPackReader,
    row_refs: &S,
    active_mask: Option<&Bitmap>,
    pack_idx: u16,
    needle: &[u8],
    case_insensitive: bool,
    read_threads: usize,
    retain: Option<([u8; 16], &RetainedPackChunks)>,
) -> Result<Bitmap, StatamError> {
    let row_count = row_refs.row_count();
    let mut results = Bitmap::new_zeroed(row_count);
    if active_mask.is_some_and(Bitmap::all_zeros) {
        return Ok(results);
    }

    if needle.is_empty() {
        visit_active_rows(row_count, active_mask, |idx| {
            if !row_refs.is_null(idx) {
                let _ = results.set(idx, true);
            }
            Ok(())
        })?;
        return Ok(results);
    }

    let mut matcher = NeedleMatcher::new(needle, case_insensitive);
    let pack_chunk_count = pack.toc().chunks.len();
    let no_edges = pack.postscript().edge_table_len == 0;

    if pack_short_circuits_all(pack, needle, case_insensitive)? {
        visit_active_rows(row_count, active_mask, |row_idx| {
            if let Some(bytes) = row_refs.inline_bytes(row_idx) {
                if matcher.matches(bytes) {
                    let _ = results.set(row_idx, true);
                }
            } else {
                let mut pack_refs = 0usize;
                row_refs.for_each_chunk(row_idx, &mut |p, cid| {
                    if p == pack_idx {
                        if (cid as usize) >= pack_chunk_count {
                            return Err(StatamError::Corrupt {
                                reason: "payload ref chunk_id out of pack bounds",
                            });
                        }
                        pack_refs += 1;
                        if no_edges && pack_refs > 1 {
                            return Err(StatamError::Corrupt {
                                reason: "multi-chunk row references pack without edge table",
                            });
                        }
                    }
                    Ok(())
                })?;
            }
            Ok(())
        })?;
        return Ok(results);
    }

    let mut chunk_rows = Vec::new();
    let mut needed_bits = vec![0u64; pack_chunk_count.div_ceil(64)];

    visit_active_rows(row_count, active_mask, |row_idx| {
        if let Some(bytes) = row_refs.inline_bytes(row_idx) {
            if matcher.matches(bytes) {
                let _ = results.set(row_idx, true);
            }
        } else if !row_refs.is_null(row_idx) {
            let mut has_pack = false;
            row_refs.for_each_chunk(row_idx, &mut |p, cid| {
                if p == pack_idx {
                    let cid_usize = cid as usize;
                    if cid_usize >= pack_chunk_count {
                        return Err(StatamError::Corrupt {
                            reason: "payload ref chunk_id out of pack bounds",
                        });
                    }
                    bitset_set(&mut needed_bits, cid_usize);
                    has_pack = true;
                }
                Ok(())
            })?;
            if has_pack {
                chunk_rows.push(row_idx);
            }
        }
        Ok(())
    })?;

    if chunk_rows.is_empty() {
        return Ok(results);
    }

    let max_match_bytes = if case_insensitive && !needle.is_ascii() {
        let mut uni_lower = String::new();
        let nl: &[u8] = if let Ok(s) = std::str::from_utf8(needle) {
            unicode_lowercase_context_free_into(s, &mut uni_lower);
            uni_lower.as_bytes()
        } else {
            needle
        };
        max_unicode_haystack_match_bytes(nl)
    } else {
        needle.len()
    };

    if max_match_bytes <= MAX_EDGE_NEEDLE_LEN {
        search_short_needle_rows(
            pack,
            row_refs,
            pack_idx,
            pack_chunk_count,
            needle,
            case_insensitive,
            read_threads,
            &mut matcher,
            &chunk_rows,
            &needed_bits,
            &mut results,
            retain,
        )?;
    } else {
        search_long_needle_rows(
            pack,
            row_refs,
            pack_idx,
            read_threads,
            &mut matcher,
            &chunk_rows,
            &needed_bits,
            &mut results,
            retain,
        )?;
    }

    Ok(results)
}

#[allow(clippy::too_many_arguments)]
fn search_short_needle_rows<S: PayloadRefSource>(
    pack: &ChunkPackReader,
    row_refs: &S,
    pack_idx: u16,
    pack_chunk_count: usize,
    needle: &[u8],
    case_insensitive: bool,
    read_threads: usize,
    matcher: &mut NeedleMatcher,
    chunk_rows: &[usize],
    needed_bits: &[u64],
    results: &mut Bitmap,
    retain: Option<([u8; 16], &RetainedPackChunks)>,
) -> Result<(), StatamError> {
    if pack.postscript().edge_table_len == 0 {
        for &row_idx in chunk_rows {
            let mut pack_refs = 0usize;
            row_refs.for_each_chunk(row_idx, &mut |p, _| {
                if p == pack_idx {
                    pack_refs += 1;
                    if pack_refs > 1 {
                        return Err(StatamError::Corrupt {
                            reason: "multi-chunk row references pack without edge table",
                        });
                    }
                }
                Ok(())
            })?;
        }
    }

    let skip_boundary = pack.postscript().edge_table_len == 0
        || (needle.len() >= 5 && pack.toc().postscript.trigram_len > 0 && {
            let ts = pack.trigram_section_cached()?;
            pack_rejects_boundary_pigeonhole(ts.edge_blooms.as_ref(), needle, case_insensitive)
        });

    // Step A: Interior matches via frame-level trigram bloom pruning + flat-bitset chunk scan.
    let surviving_chunks = filter_needed_bits_by_trigram(
        pack,
        needed_bits,
        pack_chunk_count,
        needle,
        case_insensitive,
    )?;

    if surviving_chunks.is_empty() && skip_boundary {
        return Ok(());
    }

    let mut matched_bits = vec![0u64; needed_bits.len()];
    let mut any_matched = false;
    if !surviving_chunks.is_empty() {
        if let Some((pack_id, retained)) = retain
            && retained.remaining_bytes() > 0
        {
            let toc_chunks = &pack.toc().chunks;
            let same_frame_siblings =
                SameFrameSiblings::build(row_refs, chunk_rows, pack_idx, toc_chunks);
            let frame_results = pack.scan_frames_parallel(
                &surviving_chunks,
                read_threads,
                || Ok(NeedleMatcher::new(needle, case_insensitive)),
                |m, _fid, decoded, cids| {
                    let mut matched_cids = Vec::new();
                    for &cid in cids {
                        let loc = &toc_chunks[cid as usize];
                        let start = loc.offset_in_frame as usize;
                        let end = start + loc.raw_len as usize;
                        let slice = decoded.get(start..end).ok_or(StatamError::Corrupt {
                            reason: "chunk slice out of decoded frame bounds",
                        })?;
                        if m.matches(slice) {
                            matched_cids.push(cid);
                        }
                    }
                    if matched_cids.is_empty() {
                        return Ok(None);
                    }
                    if retained.remaining_bytes() > 0 {
                        let mut kept = ReservedFrameChunks::new(retained);
                        let mut seen_in_frame: HashSet<u32> = if same_frame_siblings.is_empty() {
                            HashSet::new()
                        } else {
                            HashSet::with_capacity(matched_cids.len())
                        };
                        let mut visited_groups: HashSet<usize> = HashSet::new();
                        'retain: for &cid in &matched_cids {
                            if retained.remaining_bytes() == 0 {
                                break;
                            }
                            if same_frame_siblings.is_empty() || seen_in_frame.insert(cid) {
                                let loc = &toc_chunks[cid as usize];
                                let start = loc.offset_in_frame as usize;
                                let end = start + loc.raw_len as usize;
                                let slice = decoded.get(start..end).ok_or(StatamError::Corrupt {
                                    reason: "chunk slice out of decoded frame bounds",
                                })?;
                                let _ = kept.try_push(cid, slice);
                            }
                            if let Some(group_idxs) = same_frame_siblings.group_indices(cid) {
                                for &gidx in group_idxs {
                                    if visited_groups.insert(gidx) {
                                        for &sib_cid in same_frame_siblings.group(gidx) {
                                            if retained.remaining_bytes() == 0 {
                                                break 'retain;
                                            }
                                            if seen_in_frame.insert(sib_cid) {
                                                let loc = &toc_chunks[sib_cid as usize];
                                                let start = loc.offset_in_frame as usize;
                                                let end = start + loc.raw_len as usize;
                                                let slice = decoded.get(start..end).ok_or(
                                                    StatamError::Corrupt {
                                                        reason: "sibling chunk slice out of decoded frame bounds",
                                                    },
                                                )?;
                                                let _ = kept.try_push(sib_cid, slice);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        kept.commit(pack_id);
                    }
                    Ok(Some(matched_cids))
                },
            )?;
            for matched_cids in frame_results {
                for cid in matched_cids {
                    bitset_set(&mut matched_bits, cid as usize);
                    any_matched = true;
                }
            }
        } else if read_threads.clamp(1, crate::parallel::MAX_PARALLEL_THREADS) > 1 {
            let matched = pack.scan_chunks_in_frames_parallel(
                &surviving_chunks,
                read_threads,
                || Ok(NeedleMatcher::new(needle, case_insensitive)),
                |m, cid, bytes| m.matches(bytes).then_some(cid),
            )?;
            for cid in matched {
                bitset_set(&mut matched_bits, cid as usize);
                any_matched = true;
            }
        } else {
            pack.scan_chunks_in_frames(&surviving_chunks, |cid, bytes| {
                if matcher.matches(bytes) {
                    bitset_set(&mut matched_bits, cid as usize);
                    any_matched = true;
                }
            })?;
        }
    }

    if skip_boundary {
        if any_matched {
            for &row_idx in chunk_rows {
                if row_refs.any_chunk(row_idx, |p, cid| {
                    p == pack_idx && bitset_get(&matched_bits, cid as usize)
                }) {
                    let _ = results.set(row_idx, true);
                }
            }
        }
        return Ok(());
    }

    let mut remaining_owned = Vec::new();
    let remaining_rows: &[usize] = if any_matched {
        for &row_idx in chunk_rows {
            if row_refs.any_chunk(row_idx, |p, cid| {
                p == pack_idx && bitset_get(&matched_bits, cid as usize)
            }) {
                let _ = results.set(row_idx, true);
            } else {
                remaining_owned.push(row_idx);
            }
        }
        &remaining_owned
    } else {
        chunk_rows
    };

    if remaining_rows.is_empty() {
        return Ok(());
    }

    // Step B: Cross-chunk boundary matches using FlatEdgeTable (0 extra data frame reads).
    let mut distinct_pairs: HashSet<(u32, u32), BuildHasherDefault<FastPairHasher>> =
        HashSet::default();
    for &row_idx in remaining_rows {
        row_refs.for_each_adjacent_pair_in_pack(row_idx, pack_idx, &mut |l, r| {
            distinct_pairs.insert((l, r));
        });
    }

    if distinct_pairs.is_empty() {
        return Ok(());
    }

    let edges = pack.read_edge_table_flat()?;
    let edge_count = edges.len();
    let mut matched_pairs: HashSet<(u32, u32), BuildHasherDefault<FastPairHasher>> =
        HashSet::default();
    let mut combined = Vec::with_capacity(126);
    let is_unicode_ilike = matches!(matcher, NeedleMatcher::UnicodeIlike { .. });

    for (left_cid, right_cid) in distinct_pairs {
        if (left_cid as usize) >= edge_count {
            return Err(StatamError::Corrupt {
                reason: "left chunk_id missing from edge table",
            });
        }
        if (right_cid as usize) >= edge_count {
            return Err(StatamError::Corrupt {
                reason: "right chunk_id missing from edge table",
            });
        }
        let left_tail = edges.tail(left_cid);
        let right_head = edges.head(right_cid);

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
        for &row_idx in remaining_rows {
            if row_refs.any_adjacent_pair_in_pack(row_idx, pack_idx, |l, r| {
                matched_pairs.contains(&(l, r))
            }) {
                let _ = results.set(row_idx, true);
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn search_long_needle_rows<S: PayloadRefSource>(
    pack: &ChunkPackReader,
    row_refs: &S,
    pack_idx: u16,
    read_threads: usize,
    matcher: &mut NeedleMatcher,
    chunk_rows: &[usize],
    needed_bits: &[u64],
    results: &mut Bitmap,
    retain: Option<([u8; 16], &RetainedPackChunks)>,
) -> Result<(), StatamError> {
    let all_needed = collect_set_bits(needed_bits);
    let mut total_chunk_bytes = 0usize;
    let mut total_frame_bytes = 0usize;
    pack.check_chunks_budget(&all_needed, &mut total_chunk_bytes, &mut total_frame_bytes)?;
    let fetched: HashMap<u32, Bytes> =
        if read_threads.clamp(1, crate::parallel::MAX_PARALLEL_THREADS) > 1 {
            pack.scan_chunks_in_frames_parallel(
                &all_needed,
                read_threads,
                || Ok(()),
                |(), cid, bytes| Some((cid, Bytes::copy_from_slice(bytes))),
            )?
            .into_iter()
            .collect()
        } else {
            let mut map = HashMap::with_capacity(all_needed.len());
            pack.scan_chunks_in_frames(&all_needed, |cid, bytes| {
                map.insert(cid, Bytes::copy_from_slice(bytes));
            })?;
            map
        };

    let mut seq_cache: HashMap<SeqKey<'_>, bool> = HashMap::new();
    let mut scratch_buf = Vec::new();

    for &row_idx in chunk_rows {
        let Some(key) = row_refs.seq_key(row_idx) else {
            continue;
        };
        if let Some(&cached) = seq_cache.get(&key) {
            if cached {
                let _ = results.set(row_idx, true);
            }
            continue;
        }

        // Reconstruct contiguous runs belonging to `pack_idx` and search each run.
        let mut total_row_bytes: usize = 0;
        let mut hit = false;
        scratch_buf.clear();

        row_refs.for_each_chunk(row_idx, &mut |p, cid| {
            if hit {
                return Ok(());
            }
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
                }
                scratch_buf.clear();
            }
            Ok(())
        })?;
        if !hit && !scratch_buf.is_empty() {
            hit = matcher.matches(&scratch_buf);
        }

        seq_cache.insert(key, hit);
        if hit {
            let _ = results.set(row_idx, true);
            if let Some((pack_id, retained)) = retain
                && retained.remaining_bytes() > 0
            {
                let _ = row_refs.for_each_chunk(row_idx, &mut |p, cid| {
                    if p == pack_idx
                        && let Some(bytes) = fetched.get(&cid)
                    {
                        let _ = retained.insert_if_fits(pack_id, cid, bytes.clone());
                    }
                    Ok(())
                });
            }
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

/// Soundly extracts ASCII-lowercased trigram hashes for `ILIKE` pruning.
///
/// - If `needle.is_ascii()`, extracts all lowercased trigrams as before.
/// - If `!needle.is_ascii()`, splits `needle` into contiguous pure-ASCII byte slices excluding
///   `'k'`, `'K'`, `'s'`, `'S'` (the only ASCII letters reachable from non-ASCII scalars under
///   lowercase/case-folding: `U+212A` `K` -> `'k'`, `U+017F` `ſ` -> `'S'`), and trims a trailing
///   `'i'`/`'I'` when the run reaches the end of `needle` or precedes `"\u{0307}"` (since
///   `U+0130` `İ` -> `"i\u{0307}"`).
pub fn extract_ilike_trigram_hashes(needle: &[u8]) -> Vec<u32> {
    if needle.is_ascii() {
        let mut out = Vec::new();
        extract_trigram_hashes(needle, true, &mut out);
        return out;
    }
    extract_unicode_ilike_trigram_hashes_in_slice(needle, 0, needle.len())
}

fn extract_unicode_ilike_trigram_hashes_in_slice(
    full: &[u8],
    start: usize,
    end: usize,
) -> Vec<u32> {
    let mut out = Vec::new();
    let end = end.min(full.len());
    let mut i = start.min(end);
    while i < end {
        while i < end && !is_safe_ilike_ascii_byte(full[i]) {
            i += 1;
        }
        let run_start = i;
        while i < end && is_safe_ilike_ascii_byte(full[i]) {
            i += 1;
        }
        let mut run = &full[run_start..i];
        if (i == full.len() || full[i..].starts_with(&[0xCC, 0x87]))
            && matches!(run.last(), Some(b'i' | b'I'))
        {
            run = &run[..run.len() - 1];
        }
        if run.len() >= 3 {
            extract_trigram_hashes(run, true, &mut out);
        }
    }
    out
}

#[inline]
fn is_safe_ilike_ascii_byte(b: u8) -> bool {
    b < 0x80 && !matches!(b, b'k' | b'K' | b's' | b'S')
}

#[inline]
fn extract_needle_trigram_hashes(needle: &[u8], case_insensitive: bool) -> Vec<u32> {
    if case_insensitive {
        extract_ilike_trigram_hashes(needle)
    } else {
        let mut out = Vec::new();
        extract_trigram_hashes(needle, false, &mut out);
        out
    }
}

fn max_unicode_haystack_match_bytes(nl: &[u8]) -> usize {
    let mut total = nl.len();
    for &b in nl {
        if matches!(b, b'k' | b'K') {
            total = total.saturating_add(2);
        } else if (0xC2..=0xDF).contains(&b) {
            total = total.saturating_add(1);
        }
    }
    total
}

pub(crate) fn pack_rejects_boundary_pigeonhole(
    edge_blooms: Option<&(TrigramBloom, TrigramBloom)>,
    needle: &[u8],
    case_insensitive: bool,
) -> bool {
    let Some((tail_bloom, head_bloom)) = edge_blooms else {
        return false;
    };
    if !case_insensitive || needle.is_ascii() {
        if needle.len() < 5 || needle.len() > MAX_EDGE_NEEDLE_LEN {
            return false;
        }
        let n = needle.len();
        let half = n.div_ceil(2);
        let prefix_hashes = extract_needle_trigram_hashes(&needle[..half], case_insensitive);
        let suffix_hashes = extract_needle_trigram_hashes(&needle[n - half..], case_insensitive);
        let tail_may = tail_bloom.may_contain_hashes(&prefix_hashes);
        let head_may = head_bloom.may_contain_hashes(&suffix_hashes);
        return !tail_may && !head_may;
    }

    if needle.len() > MAX_EDGE_NEEDLE_LEN {
        return false;
    }
    let mut uni_lower = String::new();
    let nl: &[u8] = if let Ok(s) = std::str::from_utf8(needle) {
        unicode_lowercase_context_free_into(s, &mut uni_lower);
        uni_lower.as_bytes()
    } else {
        needle
    };
    if nl.len() < 5 || max_unicode_haystack_match_bytes(nl) > MAX_EDGE_NEEDLE_LEN {
        return false;
    }
    let half = nl.len().div_ceil(2);
    let prefix_hashes = extract_unicode_ilike_trigram_hashes_in_slice(nl, 0, half);
    let suffix_hashes =
        extract_unicode_ilike_trigram_hashes_in_slice(nl, nl.len() - half, nl.len());
    let tail_may = tail_bloom.may_contain_hashes(&prefix_hashes);
    let head_may = head_bloom.may_contain_hashes(&suffix_hashes);
    !tail_may && !head_may
}

pub(crate) fn pack_short_circuits_all(
    pack: &ChunkPackReader,
    needle: &[u8],
    case_insensitive: bool,
) -> Result<bool, StatamError> {
    if pack.toc().postscript.trigram_len == 0 {
        return Ok(false);
    }

    let no_edges = pack.postscript().edge_table_len == 0;
    if !no_edges && (needle.len() < 5 || needle.len() > MAX_EDGE_NEEDLE_LEN) {
        return Ok(false);
    }

    let ts = pack.trigram_section_cached()?;
    if !no_edges
        && !pack_rejects_boundary_pigeonhole(ts.edge_blooms.as_ref(), needle, case_insensitive)
    {
        return Ok(false);
    }

    let hashes = extract_needle_trigram_hashes(needle, case_insensitive);
    if hashes.is_empty() {
        return Ok(false);
    }

    let any_frame_may = ts
        .frame_blooms
        .iter()
        .any(|opt| opt.as_ref().is_none_or(|b| b.may_contain_hashes(&hashes)));
    Ok(!any_frame_may)
}

#[derive(Default)]
struct FastPairHasher(u64);

impl std::hash::Hasher for FastPairHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        for &b in bytes {
            h = (h ^ u64::from(b)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
        self.0 = h;
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.0 = (self.0 ^ u64::from(i)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 31)
    }
}

#[inline]
fn bitset_set(bits: &mut [u64], idx: usize) {
    if let Some(word) = bits.get_mut(idx >> 6) {
        *word |= 1u64 << (idx & 63);
    }
}

#[inline]
fn bitset_get(bits: &[u64], idx: usize) -> bool {
    bits.get(idx >> 6)
        .is_some_and(|&word| (word & (1u64 << (idx & 63))) != 0)
}

fn collect_set_bits(bits: &[u64]) -> Vec<u32> {
    let mut out = Vec::new();
    for (w_idx, &word) in bits.iter().enumerate() {
        let mut w = word;
        let base = (w_idx << 6) as u32;
        while w != 0 {
            let tz = w.trailing_zeros();
            out.push(base + tz);
            w &= w - 1;
        }
    }
    out
}

fn filter_needed_bits_by_trigram(
    pack: &ChunkPackReader,
    needed_bits: &[u64],
    pack_chunk_count: usize,
    needle: &[u8],
    case_insensitive: bool,
) -> Result<Vec<u32>, StatamError> {
    if pack_chunk_count == 0 {
        return Ok(Vec::new());
    }
    if pack.toc().postscript.trigram_len == 0 {
        return Ok(collect_set_bits(needed_bits));
    }

    let hashes = extract_needle_trigram_hashes(needle, case_insensitive);
    if hashes.is_empty() {
        return Ok(collect_set_bits(needed_bits));
    }

    let blooms = &pack.trigram_section_cached()?.frame_blooms;
    let frame_matches: Vec<bool> = blooms
        .iter()
        .map(|opt| match opt {
            None => true,
            Some(bloom) => bloom.may_contain_hashes(&hashes),
        })
        .collect();

    let frames = &pack.toc().frames;
    let mut surviving = Vec::new();
    if pack.toc().frames_contiguous {
        for (fid, frame) in frames.iter().enumerate() {
            if !frame_matches.get(fid).copied().unwrap_or(true) || frame.chunk_count == 0 {
                continue;
            }
            let start = frame.first_chunk_id as usize;
            let end = start + frame.chunk_count as usize;
            let first_word = start >> 6;
            let last_word = (end - 1) >> 6;
            for (w_idx, &word) in needed_bits
                .iter()
                .enumerate()
                .take(last_word + 1)
                .skip(first_word)
            {
                let mut w = word;
                if w_idx == first_word {
                    w &= !0u64 << (start & 63);
                }
                if w_idx == last_word {
                    let rem = end & 63;
                    if rem != 0 {
                        w &= (1u64 << rem) - 1;
                    }
                }
                let base = (w_idx << 6) as u32;
                while w != 0 {
                    let tz = w.trailing_zeros();
                    surviving.push(base + tz);
                    w &= w - 1;
                }
            }
        }
    } else {
        for (w_idx, &word) in needed_bits.iter().enumerate() {
            let mut w = word;
            let base = (w_idx << 6) as u32;
            while w != 0 {
                let tz = w.trailing_zeros();
                let cid = base + tz;
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
                w &= w - 1;
            }
        }
    }

    Ok(surviving)
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

    let hashes = extract_needle_trigram_hashes(needle, case_insensitive);
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
            write_threads: 1,
            ..PackWriteOptions::default()
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
            write_threads: 1,
            ..PackWriteOptions::default()
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
            write_threads: 1,
            ..PackWriteOptions::default()
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
            write_threads: 1,
            ..PackWriteOptions::default()
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
        // Under Miri, sample representative lengths so the exhaustive loop finishes in seconds.
        #[cfg(miri)]
        let lens: &[usize] = &[5, 6, 7, 8, 16, 32, 64];
        #[cfg(not(miri))]
        let lens_vec: Vec<usize> = (5..=64).collect();
        #[cfg(not(miri))]
        let lens: &[usize] = &lens_vec;

        let mut cases = Vec::new();
        for &len in lens {
            let needle = &full_needle[..len];
            #[cfg(miri)]
            let splits: Vec<usize> = vec![1, len / 2, len - 1];
            #[cfg(not(miri))]
            let splits: Vec<usize> = (1..len).collect();
            for split in splits {
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

    #[test]
    fn parallel_pack_frame_search_matches_single_threaded() {
        let opts = PackWriteOptions {
            codec: test_codec(),
            zstd_level: 3,
            target_frame_raw_bytes: 512,
            enable_trigram_skip: true,
            compress_toc_lz4: true,
            write_threads: 1,
            ..PackWriteOptions::default()
        };
        let mut builder = ChunkPackBuilder::new([0x77u8; 16], opts);
        let mut rows = Vec::new();
        for i in 0..40u32 {
            let chunk = if i % 7 == 0 {
                format!("chunk-{i}-TARGET_NEEDLE_ABC-{}", "X".repeat(300))
            } else {
                format!("chunk-{i}-ordinary-payload-{}", "Y".repeat(300))
            };
            let cid = builder.intern_chunk(chunk.as_bytes()).unwrap();
            rows.push(PayloadRefRow::Chunks(vec![(0, cid)]));
        }
        let mut pack_bytes = Vec::new();
        builder.finish(&mut pack_bytes).unwrap();
        let pack = ChunkPackReader::open(Arc::new(SimulatedS3Reader::from_bytes(
            pack_bytes,
            SimulatedS3Config::default(),
        )))
        .unwrap();
        assert!(pack.toc().frames.len() >= 4);

        let all_mask = Bitmap::new_ones(rows.len());
        let seq = search_pack_rows_masked_with_threads(
            &pack,
            &rows,
            &all_mask,
            0,
            b"target_needle_abc",
            true,
            1,
        )
        .unwrap();
        let par = search_pack_rows_masked_with_threads(
            &pack,
            &rows,
            &all_mask,
            0,
            b"target_needle_abc",
            true,
            4,
        )
        .unwrap();
        assert_eq!(seq, par);
        assert_eq!(
            par.iter_ones().collect::<Vec<_>>(),
            vec![0, 7, 14, 21, 28, 35]
        );
    }

    #[test]
    fn non_contiguous_chunk_loc_frame_id_sets_flag_false_and_avoids_false_negatives() {
        use crate::format::{FRAME_ENTRY_LEN, PACK_POSTSCRIPT_LEN};

        let opts = PackWriteOptions {
            codec: test_codec(),
            zstd_level: 3,
            target_frame_raw_bytes: 12 * 1024,
            enable_trigram_skip: true,
            compress_toc_lz4: false,
            write_threads: 1,
            ..PackWriteOptions::default()
        };
        let mut builder = ChunkPackBuilder::new([0x88u8; 16], opts);

        // Frame 0: pseudo-random 'a'..'d' (no "ALPHA_NEEDLE_99", stored_len >= 1024 so bloom emits).
        let mut state: u64 = 0x1122_3344_5566_7788;
        let mut frame0_chunk = Vec::with_capacity(13 * 1024);
        for _ in 0..(13 * 1024) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            frame0_chunk.push(b'a' + ((state >> 24) as u8 % 4));
        }

        // Frame 1: pseudo-random 'm'..'p' containing "ALPHA_NEEDLE_99".
        let mut frame1_chunk = Vec::with_capacity(13 * 1024);
        frame1_chunk.extend_from_slice(b"ALPHA_NEEDLE_99");
        for _ in 0..(13 * 1024 - 15) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            frame1_chunk.push(b'm' + ((state >> 24) as u8 % 4));
        }

        let cid0 = builder.intern_chunk(&frame0_chunk).unwrap();
        let cid1 = builder.intern_chunk(&frame1_chunk).unwrap();
        assert_eq!((cid0, cid1), (0, 1));

        let mut pack_bytes = Vec::new();
        builder.finish(&mut pack_bytes).unwrap();

        let orig_pack =
            ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes.clone()))).unwrap();
        assert!(orig_pack.toc().frames_contiguous);
        assert_eq!(orig_pack.toc().frames.len(), 2);
        let ts = orig_pack.read_trigram_section().unwrap();
        assert!(ts[0].is_some(), "expected trigram bloom on frame 0");

        // Craft a pack with the exact same frames/data/blooms, except swap ChunkLoc[0] and ChunkLoc[1]
        // while keeping frames[0] = 0..1 and frames[1] = 1..1 (`chunks[0].frame_id == 1`).
        let ps_orig = *orig_pack.postscript();
        let toc_start = pack_bytes.len() - ps_orig.toc_stored_len as usize;
        let mut crafted = pack_bytes[..toc_start].to_vec();
        let mut new_toc = Vec::with_capacity(2 * FRAME_ENTRY_LEN + 2 * 12);
        for f in &orig_pack.toc().frames {
            new_toc.extend_from_slice(&f.encode());
        }
        new_toc.extend_from_slice(&orig_pack.toc().chunks[1].encode());
        new_toc.extend_from_slice(&orig_pack.toc().chunks[0].encode());
        let new_crc = crc32c::crc32c(&new_toc);
        crafted.extend_from_slice(&new_toc);

        let mut new_ps = ps_orig;
        new_ps.toc_raw_len = new_toc.len() as u32;
        new_ps.toc_stored_len = (new_toc.len() + PACK_POSTSCRIPT_LEN) as u32;
        new_ps.toc_crc32c = new_crc;
        crafted.extend_from_slice(&new_ps.encode());

        let crafted_pack = ChunkPackReader::open(Arc::new(SliceReader::new(crafted))).unwrap();
        assert!(
            !crafted_pack.toc().frames_contiguous,
            "mismatched ChunkLoc.frame_id must set frames_contiguous = false"
        );

        let rows = vec![PayloadRefRow::Chunks(vec![(0, 0)])];
        let bm = search_pack_rows(&crafted_pack, &rows, 0, b"ALPHA_NEEDLE_99", false).unwrap();
        assert_eq!(bm.iter_ones().collect::<Vec<_>>(), vec![0]);
    }

    #[test]
    fn exhaustive_unicode_ilike_ascii_run_soundness() {
        // Mathematically prove across ALL Unicode scalar values 0..=0x10FFFF that NO non-ASCII
        // scalar has `c.to_lowercase()` producing any byte allowed inside an extracted ASCII run
        // (excluding 'k'/'K'/'s'/'S' and trailing 'i'/'I' before U+0307/EOF).
        for cp in 0u32..=0x10_FFFF {
            let Some(c) = char::from_u32(cp) else {
                continue;
            };
            if c.is_ascii() {
                continue;
            }
            for lc in c.to_lowercase() {
                if lc.is_ascii() {
                    let b = lc as u8;
                    let is_allowed_mid = is_safe_ilike_ascii_byte(b);
                    let is_i_with_combining_dot =
                        b == b'i' && c.to_lowercase().collect::<String>() == "i\u{0307}";
                    assert!(
                        !is_allowed_mid || is_i_with_combining_dot,
                        "non-ASCII U+{cp:04X} ({c}) lowercases to unsafe ASCII byte 0x{b:02X}"
                    );
                }
            }
        }
    }

    #[test]
    fn unicode_ilike_trigram_pruning_alienacion_and_flat_search_parity() {
        let mut builder = ChunkPackBuilder::new(
            [0xA1u8; 16],
            PackWriteOptions {
                target_frame_raw_bytes: 8 * 1024,
                codec: Codec::None,
                zstd_level: 1,
                enable_trigram_skip: true,
                compress_toc_lz4: false,
                write_threads: 1,
                ..PackWriteOptions::default()
            },
        );
        builder.enable_boundary_tracking();

        // Build >= 2560 B frame 0 with low trigram density containing "Alienación" and "ALIENACIÓN",
        // followed by a separate frame 1 without "alienac" so trigram pruning skips frame 1.
        let mut frame0_match = Vec::with_capacity(4096);
        let mut state = 0x1234_5678_9abc_def0u64;
        for _ in 0..4000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            frame0_match.push(b'q' + ((state >> 24) as u8 % 4));
        }
        frame0_match.extend_from_slice(" ... Alienación en progreso ... ".as_bytes());
        let cid0 = builder.intern_chunk(&frame0_match).unwrap();

        let mut frame0_upper = frame0_match.clone();
        let len_u = frame0_upper.len();
        frame0_upper.truncate(len_u - " ... Alienación en progreso ... ".len());
        frame0_upper.extend_from_slice(" ... ALIENACIÓN TOTAL ... ".as_bytes());
        let cid1 = builder.intern_chunk(&frame0_upper).unwrap();

        let mut frame1_absent = Vec::with_capacity(4096);
        for _ in 0..4096 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            frame1_absent.push(b'm' + ((state >> 24) as u8 % 4));
        }
        let cid2 = builder.intern_chunk(&frame1_absent).unwrap();
        assert_eq!((cid0, cid1, cid2), (0, 1, 2));

        let mut pack_bytes = Vec::new();
        builder.finish(&mut pack_bytes).unwrap();
        let s3 = Arc::new(SimulatedS3Reader::from_bytes(
            pack_bytes,
            SimulatedS3Config::default(),
        ));
        let pack = ChunkPackReader::open(s3.clone()).unwrap();
        assert_eq!(pack.postscript().edge_table_len, 0);

        let hashes = extract_ilike_trigram_hashes("Alienación".as_bytes());
        assert!(!hashes.is_empty());

        let rows = vec![
            PayloadRefRow::Chunks(vec![(0, 0)]),
            PayloadRefRow::Chunks(vec![(0, 1)]),
            PayloadRefRow::Chunks(vec![(0, 2)]),
            PayloadRefRow::Inline("Sin Alienación aquí".as_bytes().to_vec()),
            PayloadRefRow::Null,
        ];
        let mut encoded = Vec::new();
        crate::payload::encode_payload_refs(&rows, &mut encoded).unwrap();
        let flat =
            crate::payload::decode_payload_refs_flat(&encoded, rows.len(), 64 * 1024).unwrap();
        let mask = Bitmap::new_ones(rows.len());

        for needle in ["Alienación".as_bytes(), "ALIENACIÓN".as_bytes()] {
            let bm_rows = search_pack_rows(&pack, &rows, 0, needle, true).unwrap();
            let bm_flat = search_pack_rows_flat_masked_with_threads(
                &pack, &flat, &mask, 0, needle, true, 1, None,
            )
            .unwrap();
            assert_eq!(bm_rows.iter_ones().collect::<Vec<_>>(), vec![0, 1, 3]);
            assert_eq!(bm_flat.iter_ones().collect::<Vec<_>>(), vec![0, 1, 3]);
        }

        // Verify absent Unicode ILIKE needle short-circuits completely when edge_table_len == 0.
        let before = s3.counters();
        let bm_absent = search_pack_rows_flat_masked_with_threads(
            &pack,
            &flat,
            &mask,
            0,
            "Desaparición".as_bytes(),
            true,
            1,
            None,
        )
        .unwrap();
        let after = s3.counters();
        assert!(bm_absent.all_zeros());
        assert_eq!(
            after.gets - before.gets,
            0,
            "absent Unicode ILIKE with cached trigram section must read 0 extra frames/EdgeTable"
        );
    }

    #[test]
    fn unicode_ilike_boundary_pigeonhole_repros_and_expansion_gt_64() {
        let build_pack_and_search =
            |left_suffix: &[u8], right_prefix: &[u8], needle: &[u8]| -> Vec<usize> {
                let mut builder = ChunkPackBuilder::new(
                    [0xB2u8; 16],
                    PackWriteOptions {
                        target_frame_raw_bytes: 16 * 1024,
                        codec: Codec::None,
                        zstd_level: 1,
                        enable_trigram_skip: true,
                        compress_toc_lz4: false,
                        write_threads: 1,
                        ..PackWriteOptions::default()
                    },
                );
                let mut left = vec![b'q'; 2048];
                left.extend_from_slice(left_suffix);
                let mut right = right_prefix.to_vec();
                right.extend_from_slice(&vec![b'm'; 2048]);

                let lc = builder.intern_chunk(&left).unwrap();
                let rc = builder.intern_chunk(&right).unwrap();
                let mut pack_bytes = Vec::new();
                builder.finish(&mut pack_bytes).unwrap();

                let pack = ChunkPackReader::open(Arc::new(SimulatedS3Reader::from_bytes(
                    pack_bytes,
                    SimulatedS3Config::default(),
                )))
                .unwrap();
                let edge_blooms = pack.read_edge_trigram_blooms().unwrap();
                assert!(
                    !pack_rejects_boundary_pigeonhole(edge_blooms.as_ref(), needle, true),
                    "pigeonhole must not reject boundary match for needle {:?}",
                    String::from_utf8_lossy(needle)
                );

                let rows = vec![PayloadRefRow::Chunks(vec![(0, lc), (0, rc)])];
                let bm = search_pack_rows(&pack, &rows, 0, needle, true).unwrap();
                bm.iter_ones().collect()
            };

        // Repro A: needle = "abkdfédef" vs boundary "...abKdfÉd" || "ef..."
        assert_eq!(
            build_pack_and_search(
                "ab\u{212A}df\u{00C9}d".as_bytes(),
                b"ef",
                "abkdfédef".as_bytes(),
            ),
            vec![0]
        );

        // Repro B: needle = "xabcİdef" vs boundary "...xabcİd" || "ef..."
        assert_eq!(
            build_pack_and_search(
                "xabc\u{0130}d".as_bytes(),
                b"ef",
                "xabc\u{0130}def".as_bytes(),
            ),
            vec![0]
        );

        // Repro C: needle = "abcİdef" vs boundary "...abcİd" || "ef..."
        assert_eq!(
            build_pack_and_search(
                "abc\u{0130}d".as_bytes(),
                b"ef",
                "abc\u{0130}def".as_bytes(),
            ),
            vec![0]
        );

        // Repro #5: needle = "kkkkkkkkkkkkkkkkkkkkkké" (24 B) vs "K".repeat(22) + "é" (68 B, split 65/3)
        let haystack = format!("{}\u{00E9}", "\u{212A}".repeat(22));
        let hb = haystack.as_bytes();
        assert_eq!(hb.len(), 68);
        let needle_k = format!("{}é", "k".repeat(22));
        assert_eq!(needle_k.len(), 24);
        assert_eq!(
            build_pack_and_search(&hb[..65], &hb[65..], needle_k.as_bytes()),
            vec![0]
        );
    }

    #[test]
    fn multi_chunk_row_rejects_pack_without_edge_table() {
        let mut builder = ChunkPackBuilder::new(
            [0xC3u8; 16],
            PackWriteOptions {
                target_frame_raw_bytes: 8 * 1024,
                codec: Codec::None,
                zstd_level: 1,
                enable_trigram_skip: true,
                compress_toc_lz4: false,
                write_threads: 1,
                ..PackWriteOptions::default()
            },
        );
        builder.enable_boundary_tracking();

        let mut chunk0 = Vec::with_capacity(4096);
        let mut state = 0x9876_5432_10fe_dcba_u64;
        for _ in 0..4096 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            chunk0.push(b'a' + ((state >> 24) as u8 % 4));
        }
        let mut chunk1 = chunk0.clone();
        chunk1[0] = b'z';
        let cid0 = builder.intern_chunk(&chunk0).unwrap();
        let cid1 = builder.intern_chunk(&chunk1).unwrap();

        let mut pack_bytes = Vec::new();
        builder.finish(&mut pack_bytes).unwrap();
        let pack = ChunkPackReader::open(Arc::new(SimulatedS3Reader::from_bytes(
            pack_bytes,
            SimulatedS3Config::default(),
        )))
        .unwrap();
        assert_eq!(pack.postscript().edge_table_len, 0);

        let rows = vec![PayloadRefRow::Chunks(vec![(0, cid0), (0, cid1)])];

        // Short-circuit path (absent needle, trigram bloom rejects frame).
        let err_short = search_pack_rows(&pack, &rows, 0, b"XYZ_ABSENT_999", false).unwrap_err();
        assert!(matches!(
            err_short,
            StatamError::Corrupt {
                reason: "multi-chunk row references pack without edge table",
            }
        ));

        // Non-short-circuit path (`needle.len() < 5`, enters `search_short_needle_rows`).
        let err_non_sc = search_pack_rows(&pack, &rows, 0, b"ab", false).unwrap_err();
        assert!(matches!(
            err_non_sc,
            StatamError::Corrupt {
                reason: "multi-chunk row references pack without edge table",
            }
        ));
    }

    #[test]
    fn m11_unicode_ilike_greek_final_sigma_across_chunk_boundaries() {
        let mut builder = ChunkPackBuilder::new(
            [0x91u8; 16],
            PackWriteOptions {
                codec: test_codec(),
                target_frame_raw_bytes: 16 * 1024,
                enable_trigram_skip: true,
                ..PackWriteOptions::default()
            },
        );
        // Row 0: "...αΣ" || "a..."
        let c0_left = format!("{}αΣ", "x".repeat(300));
        let c0_right = format!("a{}", "y".repeat(300));
        let cid0_l = builder.intern_chunk(c0_left.as_bytes()).unwrap();
        let cid0_r = builder.intern_chunk(c0_right.as_bytes()).unwrap();

        // Row 1: "...αΣ" || ("." * 63 + "a")
        let c1_right = format!("{}a{}", ".".repeat(63), "z".repeat(250));
        let cid1_r = builder.intern_chunk(c1_right.as_bytes()).unwrap();

        // Row 2: "ΟΔΥΣΣΕΥΣ" chunked across two chunks.
        let c2_left = format!("{}ΟΔΥΣ", "m".repeat(300));
        let c2_right = format!("ΣΕΥΣ{}", "n".repeat(300));
        let cid2_l = builder.intern_chunk(c2_left.as_bytes()).unwrap();
        let cid2_r = builder.intern_chunk(c2_right.as_bytes()).unwrap();

        let mut pack_bytes = Vec::new();
        builder.finish(&mut pack_bytes).unwrap();
        let pack = ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes))).unwrap();

        let chunked_rows = vec![
            PayloadRefRow::Chunks(vec![(0, cid0_l), (0, cid0_r)]),
            PayloadRefRow::Chunks(vec![(0, cid0_l), (0, cid1_r)]),
            PayloadRefRow::Chunks(vec![(0, cid2_l), (0, cid2_r)]),
        ];
        let inline_rows = vec![
            PayloadRefRow::Inline(format!("{c0_left}{c0_right}").into_bytes()),
            PayloadRefRow::Inline(format!("{c0_left}{c1_right}").into_bytes()),
            PayloadRefRow::Inline(format!("{c2_left}{c2_right}").into_bytes()),
        ];

        for needle in [
            "ασa".as_bytes(),
            "αΣa".as_bytes(),
            "ας".as_bytes(),
            format!("ασ{}a", ".".repeat(63)).as_bytes(),
            "οδυσσευς".as_bytes(),
            "ς".as_bytes(),
        ] {
            let chunked_hits = search_pack_rows(&pack, &chunked_rows, 0, needle, true).unwrap();
            let inline_hits = search_pack_rows(&pack, &inline_rows, 0, needle, true).unwrap();
            assert_eq!(
                chunked_hits.iter_ones().collect::<Vec<_>>(),
                inline_hits.iter_ones().collect::<Vec<_>>(),
                "mismatch for needle {:?}",
                std::str::from_utf8(needle)
            );
        }

        // Verify `"ΟΔΥΣΣΕΥΣ" ILIKE '%ς'` hits row 2 (and `"ασa"` hits row 0).
        let hit_final_sigma = search_pack_rows(&pack, &chunked_rows, 0, "ς".as_bytes(), true)
            .unwrap()
            .iter_ones()
            .collect::<Vec<_>>();
        assert!(hit_final_sigma.contains(&2));
        let hit_split = search_pack_rows(&pack, &chunked_rows, 0, "ασa".as_bytes(), true)
            .unwrap()
            .iter_ones()
            .collect::<Vec<_>>();
        assert_eq!(hit_split, vec![0]);
    }

    #[test]
    fn search_retaining_unicode_ilike_mid_codepoint_split_matches_lossy() {
        // B1 regression: when a multi-chunk UTF-8 payload splits a codepoint across CDC chunks,
        // individual chunk slices fail `std::str::from_utf8`. Both short-needle and long-needle
        // retaining searches must match Unicode ILIKE identically to non-retaining search.
        let pack_id = [0xB1; 16];
        let mut builder = ChunkPackBuilder::new(
            pack_id,
            PackWriteOptions {
                codec: Codec::None,
                enable_trigram_skip: true,
                ..PackWriteOptions::default()
            },
        );

        // Chunk 0 ends with the first byte (0xC3) of 'é' (0xC3 0xA9), making chunk 0 invalid UTF-8
        // on its own while containing uppercase "CAFÉ" in its interior.
        let mut c0 = Vec::new();
        c0.extend_from_slice(&b"a".repeat(260));
        c0.extend_from_slice("CAFÉ ".as_bytes());
        c0.extend_from_slice(&b"b".repeat(80));
        c0.push(0xC3); // incomplete leading byte of 'é'

        // Chunk 1 starts with continuation byte 0xA9 of 'é', also invalid standalone UTF-8,
        // and contains a >64 B uppercase Unicode needle segment.
        let long_tail_upper =
            "LONG_UNICODE_NEEDLE_CAFÉ_EXCEEDING_SIXTY_FOUR_BYTES_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let long_tail_lower =
            "long_unicode_needle_café_exceeding_sixty_four_bytes_abcdefghijklmnopqrstuvwxyz";
        assert!(long_tail_lower.len() > MAX_EDGE_NEEDLE_LEN);
        let mut c1 = Vec::new();
        c1.push(0xA9); // trailing continuation byte of 'é'
        c1.extend_from_slice(&b"c".repeat(120));
        c1.extend_from_slice(long_tail_upper.as_bytes());
        c1.extend_from_slice(&b"d".repeat(120));

        // Chunk 2 has no match.
        let c2 = b"zzzzzzzzzzzzzzzz".repeat(20);

        // Verify chunks 0 and 1 are genuinely invalid standalone UTF-8, while c0 || c1 is valid UTF-8.
        assert!(std::str::from_utf8(&c0).is_err());
        assert!(std::str::from_utf8(&c1).is_err());
        let mut joined = c0.clone();
        joined.extend_from_slice(&c1);
        assert!(std::str::from_utf8(&joined).is_ok());

        let cid0 = builder.intern_chunk(&c0).unwrap();
        let cid1 = builder.intern_chunk(&c1).unwrap();
        let cid2 = builder.intern_chunk(&c2).unwrap();

        let mut pack_bytes = Vec::new();
        builder.finish(&mut pack_bytes).unwrap();
        let pack = ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes))).unwrap();

        let rows = vec![
            PayloadRefRow::Chunks(vec![(0, cid0), (0, cid1)]),
            PayloadRefRow::Chunks(vec![(0, cid2)]),
        ];
        let mut encoded = Vec::new();
        crate::payload::encode_payload_refs(&rows, &mut encoded).unwrap();
        let flat =
            crate::payload::decode_payload_refs_flat(&encoded, rows.len(), 64 * 1024).unwrap();
        let mask = Bitmap::new_ones(rows.len());

        for needle in ["café".as_bytes(), long_tail_lower.as_bytes()] {
            for threads in [1usize, 2] {
                let bm_no_retain = search_pack_rows_flat_masked_with_threads(
                    &pack, &flat, &mask, 0, needle, true, threads, None,
                )
                .unwrap();
                let retained = RetainedPackChunks::default();
                let bm_retain = search_pack_rows_flat_masked_with_threads(
                    &pack,
                    &flat,
                    &mask,
                    0,
                    needle,
                    true,
                    threads,
                    Some((pack_id, &retained)),
                )
                .unwrap();
                assert_eq!(bm_no_retain.iter_ones().collect::<Vec<_>>(), vec![0]);
                assert_eq!(bm_retain.iter_ones().collect::<Vec<_>>(), vec![0]);
                let map = retained.into_map();
                assert!(map.contains_key(&(pack_id, cid0)));
                assert!(map.contains_key(&(pack_id, cid1)));
            }
        }
    }

    #[test]
    fn retained_pack_chunks_enforces_strict_cap_across_parallel_frames() {
        // B2 regression: even across many concurrent frames matching a broad needle,
        // retained bytes never exceed the configured atomic budget.
        let pack_id = [0xB2; 16];
        let mut builder = ChunkPackBuilder::new(
            pack_id,
            PackWriteOptions {
                codec: Codec::None,
                target_frame_raw_bytes: 32 * 1024,
                enable_trigram_skip: true,
                ..PackWriteOptions::default()
            },
        );
        let mut rows = Vec::new();
        for i in 0..40u32 {
            let mut payload = format!("BROAD_HIT_{i:04}_").into_bytes();
            payload.resize(16 * 1024, b'x');
            let cid = builder.intern_chunk(&payload).unwrap();
            rows.push(PayloadRefRow::Chunks(vec![(0, cid)]));
        }
        let mut pack_bytes = Vec::new();
        builder.finish(&mut pack_bytes).unwrap();
        let pack = ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes))).unwrap();
        assert!(pack.toc().frames.len() >= 10);

        let mut encoded = Vec::new();
        crate::payload::encode_payload_refs(&rows, &mut encoded).unwrap();
        let flat = crate::payload::decode_payload_refs_flat(&encoded, rows.len(), 2 * 1024 * 1024)
            .unwrap();
        let mask = Bitmap::new_ones(rows.len());

        let cap = 64 * 1024usize;
        for threads in [1usize, 4] {
            let retained = RetainedPackChunks::new(cap);
            let bm = search_pack_rows_flat_masked_with_threads(
                &pack,
                &flat,
                &mask,
                0,
                b"BROAD_HIT_",
                false,
                threads,
                Some((pack_id, &retained)),
            )
            .unwrap();
            assert_eq!(bm.count_ones(), rows.len());
            assert!(
                retained.used_bytes() <= cap,
                "used_bytes {} exceeded cap {} (threads={threads})",
                retained.used_bytes(),
                cap
            );
            let map = retained.into_map();
            let total_retained: usize = map.values().map(Bytes::len).sum();
            assert!(total_retained <= cap);
            assert!(total_retained > 0);
        }
    }

    #[test]
    fn same_frame_siblings_high_density_row_is_linear() {
        // B3 regression: a row placing K=2_000 distinct 256 B chunks into a single frame must
        // build and query same-frame siblings in O(K) time and memory without O(K^3) blowup.
        let pack_id = [0xB3; 16];
        let mut builder = ChunkPackBuilder::new(
            pack_id,
            PackWriteOptions {
                codec: Codec::None,
                target_frame_raw_bytes: 1024 * 1024,
                enable_trigram_skip: true,
                ..PackWriteOptions::default()
            },
        );
        let k = 2_000usize;
        let mut row_refs_cids = Vec::with_capacity(k);
        for i in 0..k {
            let mut chunk = format!("SIB_HIT_{i:05}_").into_bytes();
            chunk.resize(256, b'q');
            let cid = builder.intern_chunk(&chunk).unwrap();
            row_refs_cids.push((0u16, cid));
        }
        let mut pack_bytes = Vec::new();
        builder.finish(&mut pack_bytes).unwrap();
        let pack = ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes))).unwrap();
        assert_eq!(pack.toc().frames.len(), 1);

        let rows = vec![PayloadRefRow::Chunks(row_refs_cids)];
        let mut encoded = Vec::new();
        crate::payload::encode_payload_refs(&rows, &mut encoded).unwrap();
        let flat =
            crate::payload::decode_payload_refs_flat(&encoded, rows.len(), 1024 * 1024).unwrap();
        let mask = Bitmap::new_ones(rows.len());

        let cap = 32 * 1024usize;
        let retained = RetainedPackChunks::new(cap);
        let bm = search_pack_rows_flat_masked_with_threads(
            &pack,
            &flat,
            &mask,
            0,
            b"SIB_HIT_",
            false,
            1,
            Some((pack_id, &retained)),
        )
        .unwrap();
        assert_eq!(bm.iter_ones().collect::<Vec<_>>(), vec![0]);
        assert_eq!(retained.used_bytes(), cap);
        let map = retained.into_map();
        assert_eq!(map.values().map(Bytes::len).sum::<usize>(), cap);
    }

    #[test]
    fn pass14a_unicode_lowercase_bytes_matches_scalar_edge_cases() {
        let long_ascii = "The Quick Brown Fox Jumps Over 12345 Lazy Dogs! ".repeat(4);
        assert!(long_ascii.len() > 128);
        let mixed_long = format!(
            "{}ΟΔΥΣΣΕΥΣς {}İSTANBUL {}Kelvin {}ſhort {}日本語🚀🔥 {}",
            "A".repeat(75),
            "B".repeat(67),
            "C".repeat(65),
            "D".repeat(33),
            "E".repeat(19),
            "Z".repeat(80),
        );
        let cases = [
            "",
            "a",
            "Z",
            "Hello, World! 0123456789 _-[]{}",
            long_ascii.as_str(),
            "ς",
            "Σ",
            "ΟΔΥΣΣΕΥΣ",
            "ΟΔΥΣΣΕΥς",
            "İ",
            "İSTANBUL",
            "K",
            "KELVIN",
            "ſ",
            "ſhort",
            "Alienación",
            "ALIENACIÓN",
            "日本語テキスト",
            "🚀🎉🔥",
            mixed_long.as_str(),
        ];

        let mut scalar = String::new();
        let mut fast = Vec::new();
        for s in cases {
            unicode_lowercase_context_free_into(s, &mut scalar);
            unicode_lowercase_context_free_into_bytes(s, &mut fast);
            assert_eq!(
                fast.as_slice(),
                scalar.as_bytes(),
                "mismatch for input {s:?}"
            );
        }
    }

    #[test]
    fn pass14a_needle_matcher_unicode_ilike_special_foldings_and_ascii_fast_reject() {
        // Kelvin sign U+212A lowercases to ASCII 'k' -> needle_has_non_ascii == false!
        let mut kelvin = NeedleMatcher::new("\u{212A}".as_bytes(), true);
        assert!(kelvin.matches(b"k"));
        assert!(kelvin.matches(b"K"));
        assert!(kelvin.matches("300 \u{212A}".as_bytes()));
        assert!(!kelvin.matches(b"hello world"));

        // Long s U+017F is already lowercase under `char::to_lowercase()` (only `to_uppercase()` is 'S'),
        // so `unicode_lowercase_context_free_into_bytes("ſ") == "ſ"` (non-ASCII): matches "ſong" and
        // agrees with `unicode_lowercase_context_free_into` by NOT matching ASCII "s"/"S".
        let mut long_s = NeedleMatcher::new("\u{017F}".as_bytes(), true);
        assert!(long_s.matches("\u{017F}ong".as_bytes()));
        assert!(!long_s.matches(b"s"));
        assert!(!long_s.matches(b"S"));
        assert!(!long_s.matches(b"hello world"));

        // Turkish dotted I (U+0130 -> "i\u{0307}") matches decomposed lowercase and uppercase, rejects ASCII.
        let mut dotted_i = NeedleMatcher::new("\u{0130}".as_bytes(), true);
        assert!(dotted_i.matches("i\u{0307}stanbul".as_bytes()));
        assert!(dotted_i.matches("\u{0130}STANBUL".as_bytes()));
        assert!(!dotted_i.matches(b"istanbul"));

        // Greek final sigma folds to sigma.
        let mut sigma = NeedleMatcher::new("ς".as_bytes(), true);
        assert!(sigma.matches("ΟΔΥΣΣΕΥΣ".as_bytes()));
        assert!(!sigma.matches(b"pure ascii text without greek"));

        // Non-ASCII lowercase needle fast-rejects pure-ASCII haystacks, matches Unicode.
        let mut alien = NeedleMatcher::new("Alienación".as_bytes(), true);
        assert!(!alien.matches(b"Alienacion without accent"));
        assert!(!alien.matches("PURE ASCII ".repeat(20).as_bytes()));
        assert!(alien.matches("la ALIENACIÓN moderna".as_bytes()));
        assert!(alien.matches("alienación".as_bytes()));
    }

    #[test]
    fn pass15a_exhaustive_unicode_tolower_ascii_and_caseless_cjk_invariants() {
        // 1. Exhaustive proof over all non-ASCII Unicode scalars (`0x80..=0x10_FFFF`):
        //    the ONLY scalars whose `char::to_lowercase()` emits any ASCII byte are
        //    U+0130 ('İ' -> "i\u{0307}", 'i' immediately followed by non-ASCII 0xCC)
        //    and U+212A ('K' -> "k").
        // 2. Every scalar in U+3000..=U+9FFF | U+AC00..=U+D7AF is caseless (`to_lowercase == [c]`)
        //    and 3-byte UTF-8 (`len_utf8() == 3`).
        for cp in 0x80u32..=0x10_FFFF {
            let Some(c) = char::from_u32(cp) else {
                continue;
            };
            let lowered: Vec<char> = c.to_lowercase().collect();
            if lowered.iter().any(char::is_ascii) {
                assert!(
                    c == '\u{0130}' || c == '\u{212A}',
                    "unexpected non-ASCII scalar U+{cp:04X} ({c:?}) lowercasing to ASCII: {lowered:?}"
                );
                if c == '\u{0130}' {
                    assert_eq!(lowered, vec!['i', '\u{0307}']);
                    let utf8: String = lowered.iter().collect();
                    assert_eq!(utf8.as_bytes(), &[b'i', 0xCC, 0x87]);
                } else {
                    assert_eq!(lowered, vec!['k']);
                }
            }
            if matches!(c, '\u{3000}'..='\u{9FFF}' | '\u{AC00}'..='\u{D7AF}') {
                assert_eq!(
                    lowered,
                    vec![c],
                    "expected caseless CJK/Hangul U+{cp:04X}, got {lowered:?}"
                );
                assert_eq!(c.len_utf8(), 3, "expected 3-byte UTF-8 for U+{cp:04X}");
            }
        }

        // Verify CJK/Hangul fast-copy matches scalar reference across all range boundaries
        // (including adjacent non-CJK scalars such as U+2FFF, U+A640 Cyrillic uppercase, U+D7B0).
        let boundary_str = "ABC\u{2FFF}\u{3000}日本語テスト\u{9FFF}\u{A000}\u{A640}\u{ABFF}\u{AC00}한국어테스트\u{D7AF}\u{D7B0}XYZ";
        let mut scalar = String::new();
        let mut fast = Vec::new();
        unicode_lowercase_context_free_into(boundary_str, &mut scalar);
        unicode_lowercase_context_free_into_bytes(boundary_str, &mut fast);
        assert_eq!(fast.as_slice(), scalar.as_bytes());

        // Verify anchor extraction directly on representative needles.
        let anchor_for = |needle: &str| {
            let mut raw_lower = Vec::new();
            ascii_lowercase_into(needle.as_bytes(), &mut raw_lower);
            let mut uni_lower = Vec::new();
            unicode_lowercase_context_free_into_bytes(needle, &mut uni_lower);
            extract_unicode_ilike_ascii_anchor(&uni_lower, &raw_lower)
        };
        assert_eq!(anchor_for("Alienación").as_deref(), Some(&b"alienac"[..]));
        assert_eq!(anchor_for("café").as_deref(), Some(&b"caf"[..]));
        assert_eq!(anchor_for("İSTANBUL").as_deref(), Some(&b"stanbul"[..]));
        assert_eq!(anchor_for("Kelvin").as_deref(), Some(&b"elvin"[..]));
        assert_eq!(anchor_for("ΟΔΥΣΣΕΥΣ"), None);
        assert_eq!(anchor_for("ς"), None);
        assert_eq!(anchor_for("i\u{0307}"), None);
        assert_eq!(anchor_for("aKb"), None);

        // 3. Zero false negatives on NeedleMatcher across special foldings, CJK, and invalid UTF-8.
        let mut alien = NeedleMatcher::new("Alienación".as_bytes(), true);
        assert!(alien.matches("la ALIENACIÓN moderna".as_bytes()));
        assert!(alien.matches("alienación".as_bytes()));
        assert!(!alien.matches("la ALIENACION moderna".as_bytes()));
        assert!(!alien.matches("日本語テキスト zonder match".as_bytes()));

        let mut cafe = NeedleMatcher::new("café".as_bytes(), true);
        assert!(cafe.matches("CAFÉ au lait".as_bytes()));
        assert!(cafe.matches("le café".as_bytes()));
        assert!(!cafe.matches("日本語コーヒー".as_bytes()));

        let mut istanbul = NeedleMatcher::new("İSTANBUL".as_bytes(), true);
        assert!(istanbul.matches("i\u{0307}stanbul".as_bytes()));
        assert!(istanbul.matches("visit İSTANBUL today".as_bytes()));
        assert!(!istanbul.matches("visit istanbul today".as_bytes()));

        let mut istanbul_decomp = NeedleMatcher::new("i\u{0307}stanbul".as_bytes(), true);
        assert!(istanbul_decomp.matches("İSTANBUL".as_bytes()));
        assert!(istanbul_decomp.matches("I\u{0307}STANBUL".as_bytes()));

        let mut kelvin = NeedleMatcher::new("Kelvin".as_bytes(), true);
        assert!(kelvin.matches("kelvin".as_bytes()));
        assert!(kelvin.matches("KELVIN".as_bytes()));
        assert!(kelvin.matches("300 KELVIN".as_bytes()));
        assert!(!kelvin.matches("celsius".as_bytes()));

        let mut sigma = NeedleMatcher::new("ς".as_bytes(), true);
        assert!(sigma.matches("ΟΔΥΣΣΕΥΣ".as_bytes()));
        assert!(!sigma.matches(b"odysseus"));

        // Invalid UTF-8 slices with both valid Unicode match inside lossy text AND raw-ASCII match.
        let mut invalid_lossy = vec![0xFF, 0xFE];
        invalid_lossy.extend_from_slice("prefix ALIENACIÓN suffix".as_bytes());
        invalid_lossy.push(0xC3);
        assert!(std::str::from_utf8(&invalid_lossy).is_err());
        assert!(alien.matches(&invalid_lossy));

        let mut invalid_cafe = vec![0x80];
        invalid_cafe.extend_from_slice("CAFÉ".as_bytes());
        invalid_cafe.push(0xC3);
        assert!(cafe.matches(&invalid_cafe));

        let mut invalid_raw_only = vec![0xFF];
        invalid_raw_only.extend_from_slice("ALIENACI\u{00F3}N".as_bytes());
        invalid_raw_only.push(0x80);
        assert!(alien.matches(&invalid_raw_only));

        // Trailing 'i' before U+0307 vs U+0130 in haystack: needle "abci\u{0307}" must match "ABCİ".
        let mut trailing_i = NeedleMatcher::new("abci\u{0307}".as_bytes(), true);
        assert!(trailing_i.matches("xxABC\u{0130}yy".as_bytes()));
        assert!(trailing_i.matches("xxabci\u{0307}yy".as_bytes()));
    }

    #[test]
    fn pass16c_needle_matcher_invalid_utf8_scratch_reuse_does_not_poison() {
        // UnicodeIlike WITH ascii_anchor: valid match -> invalid non-match hitting anchor ->
        // valid non-match -> valid match.
        let mut alien = NeedleMatcher::new("Alienación".as_bytes(), true);
        assert!(alien.matches("prefix ALIENACIÓN suffix".as_bytes()));
        let invalid_non_match = b"\xFF\xFEalienac\x80\xC3no-accent";
        assert!(!alien.matches(invalid_non_match));
        assert!(!alien.matches("alienacion without accent".as_bytes()));
        assert!(alien.matches("un ALIENACIÓN nuevo".as_bytes()));

        // UnicodeIlike WITHOUT ascii_anchor: valid match -> invalid non-match ->
        // invalid match -> valid non-match -> valid match.
        let mut sigma = NeedleMatcher::new("ς".as_bytes(), true);
        assert!(sigma.matches("ΟΔΥΣΣΕΥΣ".as_bytes()));
        let invalid_no_sigma = b"\xFF\xFEhello\x80world\xC3";
        assert!(!sigma.matches(invalid_no_sigma));
        let mut invalid_with_sigma = vec![0xFF, 0xFE];
        invalid_with_sigma.extend_from_slice("ΟΔΥΣΣΕΥΣ".as_bytes());
        invalid_with_sigma.push(0x80);
        assert!(sigma.matches(&invalid_with_sigma));
        assert!(!sigma.matches("αβγδε".as_bytes()));
        assert!(sigma.matches("οδυσσευς".as_bytes()));

        // AsciiIlike: valid match -> invalid UTF-8 non-match -> invalid UTF-8 match -> valid non-match.
        let mut ascii = NeedleMatcher::new(b"Needle", true);
        assert!(ascii.matches(b"haystack NEEDLE here"));
        assert!(!ascii.matches(b"\xFF\xFEnot_here\x80"));
        assert!(ascii.matches(b"\xFF\xFEhas_NeEdLe\x80"));
        assert!(!ascii.matches(b"clean ascii without match"));
        assert!(ascii.matches(b"needle"));
    }

    proptest::proptest! {
        #[test]
        fn pass14a_proptest_unicode_lowercase_bytes_parity(
            s in "([A-Za-z0-9 _\\-]{0,96}|[\\u{00C0}-\\u{04FF}\\u{212A}\\u{017F}\\u{03C2}\\u{0130}日本語🚀]{1,8}){0,8}"
        ) {
            let mut scalar = String::new();
            let mut fast = Vec::new();
            unicode_lowercase_context_free_into(&s, &mut scalar);
            unicode_lowercase_context_free_into_bytes(&s, &mut fast);
            proptest::prop_assert_eq!(fast.as_slice(), scalar.as_bytes());
        }
    }
}
