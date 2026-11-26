//! Structural JSON array + Gear-hash FastCDC payload chunker and W4 bypass detector.

/// Payloads at or below this byte length are stored inline inside `PayloadRefs`.
pub const INLINE_PAYLOAD_THRESHOLD: usize = 256;
/// Strict minimum byte length for every non-last emitted chunk.
pub const MIN_CHUNK: usize = 256;
/// Minimum cut point for Gear-hash FastCDC.
pub const CDC_MIN: usize = 512;
/// Target average chunk size for Gear-hash FastCDC.
pub const CDC_AVG: usize = 2048;
/// Maximum allowed chunk byte length.
pub const MAX_CHUNK: usize = 8192;

/// Number of initial chunks sampled before [`DedupBypassDetector`] latches its decision.
const BYPASS_SAMPLE_CHUNKS: usize = 64;
/// Minimum dedup hit rate required to keep CDC chunking enabled.
const BYPASS_MIN_HIT_RATE: f64 = 0.02;
/// Minimum compression ratio (`raw / stored`) required to keep CDC chunking enabled.
const BYPASS_MIN_RATIO: f64 = 1.05;

/// Normalized FastCDC masks over high bits of the left-shifting 64-bit Gear hash.
/// `MASK_S` has 12 bits set (used for `CDC_MIN..CDC_AVG`); `MASK_L` has 10 bits set (`CDC_AVG..max_cut`).
const MASK_S: u64 = 0x0D90_3534_0000_0000;
const MASK_L: u64 = 0x0D90_3500_0000_0000;

/// Deterministic 256-entry Gear hash lookup table generated with SplitMix64.
const GEAR: [u64; 256] = {
    let mut table = [0u64; 256];
    let mut state: u64 = 0x243f_6a88_85a3_08d3;
    let mut i = 0usize;
    while i < 256 {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        table[i] = z ^ (z >> 31);
        i += 1;
    }
    table
};

/// Clears `out` and splits `data` into contiguous slices whose concatenation equals `data`.
///
/// Invariants:
/// - `out.concat() == data` byte-for-byte.
/// - Every non-last chunk (`out[0..out.len() - 1]`) has `len >= MIN_CHUNK` (256 B)
///   and `len <= MAX_CHUNK` (8192 B).
/// - JSON arrays (`[` after optional ASCII whitespace) are split at top-level
///   element boundaries once accumulated bytes reach `MIN_CHUNK`, keeping trailing
///   `]` separate only when the preceding chunk already reached `MIN_CHUNK`.
/// - Single oversized elements (`> MAX_CHUNK`) and non-array payloads use Gear-hash
///   FastCDC (`CDC_MIN = 512`, `CDC_AVG = 2048`, `MAX_CHUNK = 8192`).
pub fn chunk_payload<'a>(data: &'a [u8], out: &mut Vec<&'a [u8]>) {
    out.clear();
    if data.is_empty() {
        return;
    }
    if data.len() <= INLINE_PAYLOAD_THRESHOLD {
        out.push(data);
        return;
    }

    let ws = data
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(data.len());
    if data.get(ws) == Some(&b'[') {
        chunk_json_array(data, ws, out);
    } else {
        fastcdc_emit(data, out);
    }
}

fn chunk_json_array<'a>(data: &'a [u8], open_bracket_idx: usize, out: &mut Vec<&'a [u8]>) {
    chunk_json_array_from(data, open_bracket_idx + 1, out);
}

/// Continues JSON-array structural chunking from an already-closed top-level element
/// boundary (e.g., `,` right after a cached prefix).
pub(crate) fn chunk_json_array_continuation<'a>(suffix: &'a [u8], out: &mut Vec<&'a [u8]>) {
    out.clear();
    chunk_json_array_from(suffix, 0, out);
}

fn chunk_json_array_from<'a>(data: &'a [u8], start_idx: usize, out: &mut Vec<&'a [u8]>) {
    let mut chunk_start = 0usize;
    let mut depth = 1usize;
    let mut in_string = false;
    let mut elem_active = false;
    let mut i = start_idx;

    while i < data.len() {
        if in_string {
            let Some(rel) = memchr::memchr2(b'"', b'\\', &data[i..]) else {
                break;
            };
            let hit_idx = i + rel;
            if data[hit_idx] == b'\\' {
                if hit_idx + 1 < data.len() {
                    i = hit_idx + 2;
                    continue;
                }
                break;
            }
            in_string = false;
            if depth == 1 {
                let end = hit_idx + 1;
                if end - chunk_start >= MIN_CHUNK {
                    fastcdc_emit(&data[chunk_start..end], out);
                    chunk_start = end;
                }
                elem_active = false;
            }
            i = hit_idx + 1;
            continue;
        }
        let b = data[i];
        {
            match b {
                b'"' => {
                    in_string = true;
                    if depth == 1 {
                        elem_active = false;
                    }
                }
                b'{' | b'[' => {
                    depth += 1;
                    if depth == 2 {
                        elem_active = false;
                    }
                }
                b'}' | b']' => {
                    if depth > 1 {
                        depth -= 1;
                        if depth == 1 {
                            let end = i + 1;
                            if end - chunk_start >= MIN_CHUNK {
                                fastcdc_emit(&data[chunk_start..end], out);
                                chunk_start = end;
                            }
                            elem_active = false;
                        }
                    } else {
                        // Closing the top-level `[` (or malformed extra `}`).
                        // Split before `]` only if the accumulated slice already has >= MIN_CHUNK.
                        if i - chunk_start >= MIN_CHUNK {
                            fastcdc_emit(&data[chunk_start..i], out);
                            chunk_start = i;
                        }
                        let _ = elem_active;
                        break;
                    }
                }
                b',' if depth == 1 => {
                    if elem_active && i - chunk_start >= MIN_CHUNK {
                        fastcdc_emit(&data[chunk_start..i], out);
                        chunk_start = i;
                    }
                    elem_active = false;
                }
                _ if depth == 1 && !b.is_ascii_whitespace() => {
                    elem_active = true;
                }
                _ => {}
            }
        }
        i += 1;
    }

    if chunk_start < data.len() {
        let rem = &data[chunk_start..];
        if rem.len() > MAX_CHUNK {
            fastcdc_emit(rem, out);
        } else {
            out.push(rem);
        }
    }
}

/// Emits `slice` chunked via Gear-hash FastCDC.
///
/// When `slice.len() >= MIN_CHUNK`, every emitted chunk (including the last) has
/// `MIN_CHUNK <= len <= MAX_CHUNK`.
fn fastcdc_emit<'a>(slice: &'a [u8], out: &mut Vec<&'a [u8]>) {
    let mut rest = slice;
    while rest.len() > MAX_CHUNK {
        let cut = fastcdc_cut(rest);
        let (head, tail) = rest.split_at(cut);
        out.push(head);
        rest = tail;
    }
    if !rest.is_empty() {
        out.push(rest);
    }
}

/// Chooses a Gear-hash cut point in `CDC_MIN..=max_cut` for `slice.len() > MAX_CHUNK`,
/// guaranteeing both the emitted prefix and the remaining tail have `len >= MIN_CHUNK`.
fn fastcdc_cut(slice: &[u8]) -> usize {
    let rem = slice.len();
    debug_assert!(rem > MAX_CHUNK);

    let max_cut = if rem <= MAX_CHUNK + MIN_CHUNK {
        rem - MIN_CHUNK
    } else {
        MAX_CHUNK
    };
    let normal_end = CDC_AVG.min(max_cut);

    let mut h: u64 = 0;
    let warmup_start = CDC_MIN.saturating_sub(64);
    for &b in &slice[warmup_start..CDC_MIN] {
        h = (h << 1).wrapping_add(GEAR[b as usize]);
    }
    for (idx, &b) in slice[CDC_MIN..normal_end].iter().enumerate() {
        h = (h << 1).wrapping_add(GEAR[b as usize]);
        if (h & MASK_S) == 0 {
            return CDC_MIN + idx + 1;
        }
    }
    for (idx, &b) in slice[normal_end..max_cut].iter().enumerate() {
        h = (h << 1).wrapping_add(GEAR[b as usize]);
        if (h & MASK_L) == 0 {
            return normal_end + idx + 1;
        }
    }
    max_cut
}

/// Tracks dedup hit rate and compressibility over the first 64 chunks of a column
/// to detect adversarial incompressible `W4` payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupBypassDetector {
    total_chunks: usize,
    dedup_hits: usize,
    raw_bytes: u64,
    stored_bytes: u64,
    decided: bool,
    bypassed: bool,
}

impl Default for DedupBypassDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl DedupBypassDetector {
    /// Creates a new detector with zero observations.
    pub fn new() -> Self {
        Self {
            total_chunks: 0,
            dedup_hits: 0,
            raw_bytes: 0,
            stored_bytes: 0,
            decided: false,
            bypassed: false,
        }
    }

    /// Records one chunk observation (`is_dedup_hit`, uncompressed size, compressed size).
    ///
    /// Once 64 chunks have been observed, latches `should_bypass() == true` iff
    /// `hit_rate < 0.02 && compression_ratio < 1.05`.
    pub fn record(&mut self, is_dedup_hit: bool, raw_len: usize, stored_len: usize) {
        self.total_chunks = self.total_chunks.saturating_add(1);
        if is_dedup_hit {
            self.dedup_hits = self.dedup_hits.saturating_add(1);
        }
        self.raw_bytes = self.raw_bytes.saturating_add(raw_len as u64);
        self.stored_bytes = self.stored_bytes.saturating_add(stored_len as u64);

        if !self.decided && self.total_chunks >= BYPASS_SAMPLE_CHUNKS {
            self.decided = true;
            let hit_rate = self.hit_rate();
            let ratio = self.compression_ratio();
            self.bypassed = hit_rate < BYPASS_MIN_HIT_RATE && ratio < BYPASS_MIN_RATIO;
        }
    }

    /// Returns `true` if adversarial incompressible `W4` bypass was triggered.
    #[inline]
    pub fn should_bypass(&self) -> bool {
        self.bypassed
    }

    /// Returns `true` once 64 chunks have been sampled.
    #[inline]
    pub fn is_decided(&self) -> bool {
        self.decided
    }

    /// Total chunks observed so far.
    #[inline]
    pub fn total_chunks(&self) -> usize {
        self.total_chunks
    }

    /// Observed deduplication hit rate in `0.0..=1.0`.
    pub fn hit_rate(&self) -> f64 {
        if self.total_chunks == 0 {
            return 0.0;
        }
        (self.dedup_hits as f64) / (self.total_chunks as f64)
    }

    /// Observed compression ratio (`raw_bytes / stored_bytes`).
    pub fn compression_ratio(&self) -> f64 {
        if self.stored_bytes == 0 {
            return 1.0;
        }
        (self.raw_bytes as f64) / (self.stored_bytes as f64)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn assert_chunk_invariants(data: &[u8], chunks: &[&[u8]]) {
        let reconstructed: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
        assert_eq!(reconstructed, data, "chunks must concatenate to input");
        if data.is_empty() {
            assert!(chunks.is_empty());
            return;
        }
        assert!(!chunks.is_empty());
        for (idx, c) in chunks[..chunks.len() - 1].iter().enumerate() {
            assert!(
                c.len() >= MIN_CHUNK,
                "non-last chunk {idx} had len {} < {MIN_CHUNK}",
                c.len()
            );
            assert!(
                c.len() <= MAX_CHUNK,
                "non-last chunk {idx} had len {} > {MAX_CHUNK}",
                c.len()
            );
        }
        assert!(chunks[chunks.len() - 1].len() <= MAX_CHUNK);
    }

    #[test]
    fn chunker_json_array_reuses_prefix_and_enforces_256b_non_last_invariant() {
        let turn0 = format!("{{\"role\":\"user\",\"content\":\"{}\"}}", "U".repeat(300));
        let turn1 = format!(
            "{{\"role\":\"assistant\",\"content\":\"{}\"}}",
            "A".repeat(320)
        );
        let turn2 = format!(
            "{{\"role\":\"user\",\"content\":\"{}\"}}",
            "Follow-up question: ".repeat(18)
        );

        let conv_k = format!("[{turn0},{turn1}]");
        let conv_k1 = format!("[{turn0},{turn1},{turn2}]");

        let mut chunks_k = Vec::new();
        chunk_payload(conv_k.as_bytes(), &mut chunks_k);
        assert_chunk_invariants(conv_k.as_bytes(), &chunks_k);

        let mut chunks_k1 = Vec::new();
        chunk_payload(conv_k1.as_bytes(), &mut chunks_k1);
        assert_chunk_invariants(conv_k1.as_bytes(), &chunks_k1);

        // Turn k has [ [turn0, ,turn1, ] ], so Turn k+1 reuses BOTH turn0 AND turn1 verbatim!
        assert_eq!(chunks_k.len(), 3);
        assert_eq!(chunks_k1.len(), 4);
        assert_eq!(chunks_k[0], chunks_k1[0]);
        assert_eq!(chunks_k[1], chunks_k1[1]);
        assert_eq!(chunks_k[2], b"]");
        assert_eq!(chunks_k1[3], b"]");

        // Small element followed by `]` where accumulated < 256 B keeps `]` attached.
        let small_tail = format!("[{turn0},{{\"role\":\"user\",\"content\":\"ok\"}}]");
        let mut small_chunks = Vec::new();
        chunk_payload(small_tail.as_bytes(), &mut small_chunks);
        assert_chunk_invariants(small_tail.as_bytes(), &small_chunks);
        assert_eq!(small_chunks.len(), 2);
        assert!(small_chunks[0].len() >= MIN_CHUNK);
        assert!(small_chunks[1].ends_with(b"]"));
    }

    #[test]
    fn chunker_continuation_matches_full_chunk_payload_for_cumulative_arrays() {
        let m0 = format!("{{\"role\":\"user\",\"content\":\"{}\"}}", "A".repeat(300));
        let m1 = format!(
            "{{\"role\":\"assistant\",\"content\":\"{}\"}}",
            "B".repeat(320)
        );
        // Include an oversized (> 8 KiB) element to exercise FastCDC inside continuation.
        let m2_big = format!(
            "{{\"role\":\"tool\",\"content\":\"{}\"}}",
            "C123456789".repeat(1_200)
        );
        let m3_small = "{\"role\":\"user\",\"content\":\"ok\"}".to_owned();
        let m4 = format!(
            "{{\"role\":\"assistant\",\"content\":\"{}\"}}",
            "D".repeat(400)
        );

        let turns = [
            format!("[{m0}]"),
            format!("[{m0},{m1}]"),
            format!("[{m0},{m1},{m2_big}]"),
            format!("[{m0},{m1},{m2_big},{m3_small}]"),
            format!("[{m0},{m1},{m2_big},{m3_small},{m4}]"),
        ];

        let mut prev_bytes: Option<Vec<u8>> = None;
        let mut prev_chunks: Vec<Vec<u8>> = Vec::new();
        let mut cont_scratch: Vec<&[u8]> = Vec::new();
        let mut full_scratch: Vec<&[u8]> = Vec::new();

        for cur_str in &turns {
            let cur = cur_str.as_bytes();
            chunk_payload(cur, &mut full_scratch);
            assert_chunk_invariants(cur, &full_scratch);

            if let Some(prev) = &prev_bytes
                && let Some(last_chunk) = prev_chunks.last().map(Vec::as_slice)
                && (last_chunk == b"]".as_slice() || last_chunk.starts_with(b","))
                && prev_chunks.len() >= 2
                && prev.ends_with(b"]")
                && prev.len() > last_chunk.len()
            {
                let prefix_len = prev.len() - last_chunk.len();
                assert_eq!(cur[prefix_len], b',');
                chunk_json_array_continuation(&cur[prefix_len..], &mut cont_scratch);

                let mut combined: Vec<&[u8]> = prev_chunks[..prev_chunks.len() - 1]
                    .iter()
                    .map(Vec::as_slice)
                    .collect();
                combined.extend(cont_scratch.iter().copied());
                assert_eq!(
                    combined, full_scratch,
                    "continuation must produce 100% identical chunks"
                );
            }

            prev_bytes = Some(cur.to_vec());
            prev_chunks = full_scratch.iter().map(|s| s.to_vec()).collect();
        }
    }

    #[test]
    fn chunker_malformed_json_and_oversized_elements_roundtrip_exact() {
        // 1. Huge single JSON object element (> MAX_CHUNK) + tiny tail element.
        let big_elem = format!("{{\"huge\":\"{}\"}}", "X".repeat(25_000));
        let tiny_elem = "{\"ok\":true}";
        let json_big = format!("  \n[{big_elem},{tiny_elem}]");
        let mut out = Vec::new();
        chunk_payload(json_big.as_bytes(), &mut out);
        assert_chunk_invariants(json_big.as_bytes(), &out);
        assert!(out.len() >= 3);

        // 2. Malformed JSON with unclosed string exceeding MAX_CHUNK.
        let unclosed = format!("[{{\"broken\":\"{}", "Z".repeat(20_000));
        chunk_payload(unclosed.as_bytes(), &mut out);
        assert_chunk_invariants(unclosed.as_bytes(), &out);

        // 3. Slice right inside the edge window (MAX_CHUNK < L <= MAX_CHUNK + MIN_CHUNK).
        for len in [
            MAX_CHUNK + 1,
            MAX_CHUNK + 128,
            MAX_CHUNK + MIN_CHUNK,
            MAX_CHUNK + MIN_CHUNK + 1,
        ] {
            let mut raw = vec![b'['];
            raw.extend(std::iter::repeat_n(b'a', len - 1));
            let mut raw_out = Vec::new();
            chunk_payload(&raw, &mut raw_out);
            assert_chunk_invariants(&raw, &raw_out);

            let plain = vec![b'b'; len];
            let mut plain_out = Vec::new();
            chunk_payload(&plain, &mut plain_out);
            assert_chunk_invariants(&plain, &plain_out);
        }

        // 4. Malformed extra closing brackets + trailing bytes > MAX_CHUNK.
        let extra_close = format!(
            "[{{\"a\":\"{}\"}}]]]{}",
            "q".repeat(300),
            "r".repeat(12_000)
        );
        chunk_payload(extra_close.as_bytes(), &mut out);
        assert_chunk_invariants(extra_close.as_bytes(), &out);
    }

    #[test]
    fn dedup_bypass_detector_triggers_only_on_incompressible_unique_stream() {
        let mut w4 = DedupBypassDetector::new();
        for _ in 0..63 {
            w4.record(false, 2048, 2048);
            assert!(!w4.is_decided());
            assert!(!w4.should_bypass());
        }
        w4.record(false, 2048, 2048);
        assert!(w4.is_decided());
        assert!(w4.should_bypass());

        // High compressibility prevents bypass even with 0% dedup.
        let mut compressible = DedupBypassDetector::new();
        for _ in 0..64 {
            compressible.record(false, 2048, 512);
        }
        assert!(compressible.is_decided());
        assert!(!compressible.should_bypass());

        // >= 2% dedup hit rate prevents bypass even if incompressible.
        let mut dedup_ok = DedupBypassDetector::new();
        for i in 0..64 {
            dedup_ok.record(i < 2, 2048, 2048);
        }
        assert!(dedup_ok.is_decided());
        assert!(!dedup_ok.should_bypass());
    }

    proptest! {
        #[test]
        fn proptest_chunker_exact_concat_and_256b_non_last(
            raw in prop::collection::vec(any::<u8>(), 0..=20_000),
            as_json_prefix in any::<bool>(),
            elements in prop::collection::vec(prop::collection::vec(b'a'..=b'z', 1..=600), 1..=12),
        ) {
            let mut out = Vec::new();

            // 1. Arbitrary random bytes
            chunk_payload(&raw, &mut out);
            assert_chunk_invariants(&raw, &out);

            // 2. Arbitrary bytes prefixed with `[` (malformed JSON stress)
            if as_json_prefix {
                let mut prefixed = Vec::with_capacity(raw.len() + 1);
                prefixed.push(b'[');
                prefixed.extend_from_slice(&raw);
                let mut prefixed_out = Vec::new();
                chunk_payload(&prefixed, &mut prefixed_out);
                assert_chunk_invariants(&prefixed, &prefixed_out);
            }

            // 3. Valid JSON arrays of arbitrary element lengths + growing conversation turn k -> k+1
            let mut conv_k = Vec::from(b"[".as_slice());
            for (idx, el) in elements.iter().enumerate() {
                if idx > 0 {
                    conv_k.push(b',');
                }
                conv_k.extend_from_slice(b"{\"m\":\"");
                conv_k.extend_from_slice(el);
                conv_k.extend_from_slice(b"\"}");
            }
            let mut conv_k1 = conv_k.clone();
            conv_k.push(b']');
            chunk_payload(&conv_k, &mut out);
            assert_chunk_invariants(&conv_k, &out);

            conv_k1.extend_from_slice(b",{\"m\":\"");
            conv_k1.extend_from_slice(&vec![b'z'; 300]);
            conv_k1.extend_from_slice(b"\"}]");
            let mut out_k1 = Vec::new();
            chunk_payload(&conv_k1, &mut out_k1);
            assert_chunk_invariants(&conv_k1, &out_k1);
        }
    }
}
