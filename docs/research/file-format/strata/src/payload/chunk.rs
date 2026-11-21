//! Value chunking (S4 steps 1-5): cuts one payload value into chunks.
//!
//! A JSON array is first split into its top-level elements. Long elements are cut
//! by a gear hash, short ones stay whole. Adjacent pieces are then merged until each
//! chunk is at least `MIN_CHUNK` bytes, except the last.

use std::ops::Range;

/// Minimum size of every chunk of a value except its last (S4 step 5).
pub(crate) const MIN_CHUNK: usize = 256;
/// A JSON unit at least this long is cut by the gear chunker (S4 step 2).
pub(crate) const GEAR_UNIT_THRESHOLD: usize = 2048;
/// Gear chunker parameters (S4 step 2): pieces of 256 B to 4 KiB, about 1 KiB on average.
pub(crate) const GEAR_MIN: usize = 256;
pub(crate) const GEAR_MAX: usize = 4096;
pub(crate) const GEAR_MASK: u64 = (1 << 10) - 1;
const GEAR_SEED: u64 = 0x1234_5678_9abc_def1;

#[allow(clippy::indexing_slicing)] // i < 256 by the loop bound
const fn gear_table() -> [u64; 256] {
    let mut t = [0u64; 256];
    let mut x: u64 = GEAR_SEED;
    let mut i = 0;
    while i < 256 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        t[i] = x;
        i += 1;
    }
    t
}

static GEAR_TABLE: [u64; 256] = gear_table();

fn gear_entry(b: u8) -> u64 {
    GEAR_TABLE.get(usize::from(b)).copied().unwrap_or(0)
}

/// Appends piece ranges covering `data` exactly, shifted by `base`.
fn gear_pieces(data: &[u8], out: &mut Vec<Range<usize>>, base: usize) {
    let mut start = 0usize;
    while start < data.len() {
        let Some(rest) = data.get(start..) else { break }; // never breaks: start < len
        if rest.len() <= GEAR_MIN {
            out.push(base + start..base + data.len());
            break;
        }
        let mut h: u64 = 0;
        let mut cut = rest.len().min(GEAR_MAX);
        for (i, &b) in rest.iter().enumerate().take(GEAR_MAX).skip(GEAR_MIN) {
            h = (h << 1).wrapping_add(gear_entry(b));
            if (h >> 52) & GEAR_MASK == 0 {
                cut = i + 1;
                break;
            }
        }
        // A cut moves back to the start of a UTF-8 character, so that the chunks of a UTF-8
        // value are UTF-8 too (a character is at most 4 bytes). It stays content-defined, and
        // the piece stays at least `GEAR_MIN - 2` bytes long.
        let mut back = 0;
        while back < 3 && rest.get(cut).is_some_and(|&b| b & 0xC0 == 0x80) {
            cut -= 1;
            back += 1;
        }
        out.push(base + start..base + start + cut);
        start += cut;
    }
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// Appends to `cuts` the cut positions of a JSON array from `from` on, where the scanner is at
/// depth 1 and outside a string: the absolute indexes where a new top-level element starts.
/// Returns `false` when the rest of `value` is not the rest of a JSON array the scanner accepts.
/// Strings are skipped with `memchr`, so the scan costs little per byte of text.
fn json_cuts_from(value: &[u8], from: usize, cuts: &mut Vec<usize>) -> bool {
    let mut depth = 1u32;
    let mut i = from;
    while let Some(&c) = value.get(i) {
        match c {
            b'"' => {
                // Skip the string: the next unescaped quote ends it.
                i += 1;
                loop {
                    let Some(rest) = value.get(i..) else {
                        return false;
                    };
                    let Some(at) = memchr::memchr2(b'"', b'\\', rest) else {
                        return false;
                    };
                    i += at;
                    if value.get(i) == Some(&b'\\') {
                        i += 2;
                    } else {
                        break;
                    }
                }
            }
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                depth -= 1; // depth ≥ 1 here, so no underflow
                if depth == 0 {
                    let tail_ok = value
                        .get(i + 1..)
                        .is_some_and(|t| t.iter().all(|&b| is_ws(b)));
                    return c == b']' && tail_ok;
                }
            }
            b',' if depth == 1 => cuts.push(i + 1),
            _ => {}
        }
        i += 1;
    }
    false // ended at depth > 0 or inside a string
}

/// One chunk of a value: its byte range, and whether it ends at a top-level cut of a JSON
/// array, where chunking can start again with [`chunk_value_from`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Chunk {
    pub range: Range<usize>,
    pub at_cut: bool,
}

/// Cuts `value[from..]` into chunks, where `from` is 0 or the end of a chunk that ended at a
/// top-level cut. Returns `None` when `from` is not 0 and the rest is not the rest of a JSON
/// array. The chunks are contiguous, non-empty, cover `from..value.len()`, and every chunk but
/// the last is at least `MIN_CHUNK` bytes.
pub(crate) fn chunk_value_from(value: &[u8], from: usize) -> Option<Vec<Chunk>> {
    if from >= value.len() {
        return (from == value.len()).then(Vec::new);
    }
    // Pieces, and whether each ends at a top-level cut.
    let mut pieces: Vec<(Range<usize>, bool)> = Vec::new();
    let mut cuts = Vec::new();
    let json = if from == 0 {
        match value.iter().position(|&b| !is_ws(b)) {
            Some(first) if value.get(first) == Some(&b'[') => {
                json_cuts_from(value, first + 1, &mut cuts)
            }
            _ => false,
        }
    } else {
        json_cuts_from(value, from, &mut cuts)
    };
    if json {
        let mut unit_start = from;
        let n_cuts = cuts.len();
        for (k, end) in cuts
            .iter()
            .copied()
            .chain(std::iter::once(value.len()))
            .enumerate()
        {
            let at_cut = k < n_cuts;
            let unit = unit_start..end;
            if unit.len() < GEAR_UNIT_THRESHOLD {
                pieces.push((unit, at_cut));
            } else {
                let data = value.get(unit.clone()).unwrap_or_default();
                let mut gear = Vec::new();
                gear_pieces(data, &mut gear, unit.start);
                let n = gear.len();
                pieces.extend(
                    gear.into_iter()
                        .enumerate()
                        .map(|(i, r)| (r, at_cut && i + 1 == n)),
                );
            }
            unit_start = end;
        }
    } else if from == 0 {
        let mut gear = Vec::new();
        gear_pieces(value, &mut gear, 0);
        pieces.extend(gear.into_iter().map(|r| (r, false)));
    } else {
        return None;
    }

    let mut out = Vec::new();
    let mut chunk_start = from;
    for (p, at_cut) in &pieces {
        if p.end - chunk_start >= MIN_CHUNK {
            out.push(Chunk {
                range: chunk_start..p.end,
                at_cut: *at_cut,
            });
            chunk_start = p.end;
        }
    }
    if chunk_start < value.len() {
        out.push(Chunk {
            range: chunk_start..value.len(),
            at_cut: false,
        });
    }
    Some(out)
}

/// Cuts `value` into chunk ranges. The ranges are contiguous, non-empty, cover
/// `0..value.len()`, and every range but the last is at least `MIN_CHUNK` bytes.
#[cfg(test)]
pub(crate) fn chunk_value(value: &[u8]) -> Vec<Range<usize>> {
    chunk_value_from(value, 0)
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.range)
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// The reference chunker: `bench/src/payload.rs::Gear` with the format's gear parameters,
    /// and cuts moved back to the start of a UTF-8 character.
    struct RefGear([u64; 256]);

    impl RefGear {
        fn new() -> Self {
            let mut t = [0u64; 256];
            let mut x: u64 = 0x1234_5678_9abc_def1;
            for v in &mut t {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *v = x;
            }
            Self(t)
        }

        fn chunks<'a>(&self, data: &'a [u8], out: &mut Vec<&'a [u8]>) {
            const MIN: usize = 256;
            const MAX: usize = 4096;
            const MASK: u64 = (1 << 10) - 1;
            let mut start = 0;
            while start < data.len() {
                let rest = &data[start..];
                if rest.len() <= MIN {
                    out.push(rest);
                    break;
                }
                let mut h: u64 = 0;
                let mut cut = rest.len().min(MAX);
                for (i, &b) in rest.iter().enumerate().take(MAX).skip(MIN) {
                    h = (h << 1).wrapping_add(self.0[b as usize]);
                    if (h >> 52) & MASK == 0 {
                        cut = i + 1;
                        break;
                    }
                }
                for _ in 0..3 {
                    if cut < rest.len() && rest[cut] & 0xC0 == 0x80 {
                        cut -= 1;
                    }
                }
                out.push(&rest[..cut]);
                start += cut;
            }
        }
    }

    /// Reference pieces of `data` as ranges, from `RefGear::chunks`.
    fn ref_pieces(data: &[u8]) -> Vec<Range<usize>> {
        let mut slices = Vec::new();
        RefGear::new().chunks(data, &mut slices);
        let mut at = 0usize;
        slices
            .iter()
            .map(|s| {
                let r = at..at + s.len();
                at = r.end;
                r
            })
            .collect()
    }

    /// Expected `chunk_value` output for a value with no JSON structure: the same
    /// accumulation over the reference pieces.
    fn ref_chunk_value(value: &[u8]) -> Vec<Range<usize>> {
        if value.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut chunk_start = 0usize;
        for p in ref_pieces(value) {
            if p.end - chunk_start >= MIN_CHUNK {
                out.push(chunk_start..p.end);
                chunk_start = p.end;
            }
        }
        if chunk_start < value.len() {
            out.push(chunk_start..value.len());
        }
        out
    }

    fn join(value: &[u8], chunks: &[Range<usize>]) -> Vec<u8> {
        chunks
            .iter()
            .flat_map(|c| value.get(c.clone()).unwrap_or_default().iter().copied())
            .collect()
    }

    fn gear_input() -> impl Strategy<Value = Vec<u8>> {
        prop_oneof![
            proptest::collection::vec(
                proptest::sample::select(b"ab {}[],\"\n".to_vec()),
                0..40_000
            ),
            proptest::collection::vec(any::<u8>(), 0..40_000),
        ]
    }

    const JSON_CHARS: &[char] = &['a', '"', '\\', ',', '[', ']', '{', '}', '\n', 'é', '日'];

    fn json_string() -> impl Strategy<Value = String> {
        proptest::collection::vec(proptest::sample::select(JSON_CHARS.to_vec()), 0..3000)
            .prop_map(|cs: Vec<char>| cs.into_iter().collect::<String>())
    }

    fn json_element() -> impl Strategy<Value = serde_json::Value> {
        prop_oneof![
            json_string().prop_map(serde_json::Value::String),
            proptest::collection::vec(any::<i64>(), 0..=3).prop_map(|v| {
                serde_json::Value::Array(v.into_iter().map(serde_json::Value::from).collect())
            }),
            json_string().prop_map(|s| serde_json::json!({ "k": s })),
            any::<i64>().prop_map(serde_json::Value::from),
        ]
    }

    fn json_array() -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(json_element(), 0..40)
            .prop_map(|v| serde_json::to_string(&v).unwrap_or_default().into_bytes())
    }

    /// JSON arrays of one to five strings, each of 8,000 to 19,999 bytes. The repeated
    /// strings keep debug time low, and the elements straddle the gear threshold, so the
    /// gear-inside-JSON branch runs with a non-zero base.
    fn long_string_array() -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(8_000usize..20_000, 1..6).prop_map(|lens| {
            let items: Vec<String> = lens.iter().map(|k| "x".repeat(*k)).collect();
            serde_json::to_string(&items)
                .unwrap_or_default()
                .into_bytes()
        })
    }

    fn value_strategy() -> impl Strategy<Value = Vec<u8>> {
        prop_oneof![
            json_array(),
            proptest::collection::vec(any::<u8>(), 0..50_000),
            long_string_array(),
        ]
    }

    #[test]
    fn gear_pieces_match_reference_fixed_cases() {
        let cases: Vec<(Vec<u8>, Vec<usize>)> = vec![
            (vec![], vec![]),
            (vec![0; 256], vec![256]),
            (vec![0; 257], vec![257]),
            (vec![0; 4096], vec![4096]),
            (vec![0; 4097], vec![4096, 1]),
            (vec![0; 9000], vec![4096, 4096, 808]),
        ];
        for (data, want) in cases {
            let mut out = Vec::new();
            gear_pieces(&data, &mut out, 0);
            let lens: Vec<usize> = out.iter().map(|r| r.len()).collect();
            assert_eq!(lens, want);
            assert_eq!(out, ref_pieces(&data));
        }
    }

    #[test]
    fn gear_pieces_cut_between_characters() {
        // Two-, three- and four-byte characters, long enough for many gear cuts.
        // Pseudo-random, so the gear hash cuts by content and not only at `GEAR_MAX`.
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let text: String = (0..12_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                ['é', '日', '😀', 'a'][(x >> 32) as usize % 4]
            })
            .collect();
        let mut out = Vec::new();
        gear_pieces(text.as_bytes(), &mut out, 0);
        assert!(out.len() > 15, "{} pieces", out.len());
        for r in &out {
            assert!(
                std::str::from_utf8(&text.as_bytes()[r.clone()]).is_ok(),
                "{r:?}"
            );
        }
        assert_eq!(out, ref_pieces(text.as_bytes()));
    }

    #[test]
    fn gear_pieces_base_shifts_ranges() {
        let data = vec![0u8; 9000];
        let mut out = Vec::new();
        gear_pieces(&data, &mut out, 1000);
        assert_eq!(out, vec![1000..5096, 5096..9192, 9192..10000]);
    }

    /// The top-level cut positions of a JSON array, or `None` when it is not one.
    fn json_units(value: &[u8]) -> Option<Vec<usize>> {
        let first = value.iter().position(|&b| !is_ws(b))?;
        if value.get(first) != Some(&b'[') {
            return None;
        }
        let mut cuts = Vec::new();
        json_cuts_from(value, first + 1, &mut cuts).then_some(cuts)
    }

    /// A JSON array of `n` messages of 100 to 3,000 bytes; one in 5 is over 8 KiB.
    fn messages(n: usize, seed: u64) -> Vec<String> {
        let mut x = seed | 1;
        (0..n)
            .map(|i| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let len = if i % 5 == 4 {
                    9_000 + (x % 4_000) as usize
                } else {
                    100 + (x % 2_900) as usize
                };
                let text: String = (0..len)
                    .map(|j| char::from(b'a' + ((x >> (j % 50)) as u8).wrapping_add(j as u8) % 26))
                    .collect();
                format!("{{\"role\":\"user\",\"content\":\"{text} \\\"q\\\" {i}\"}}")
            })
            .collect()
    }

    #[test]
    fn chunking_from_a_reused_cut_matches_chunking_the_whole_value() {
        let msgs = messages(40, 7);
        for split in [3usize, 10, 25] {
            let prev = format!("[{}]", msgs[..split].join(","));
            let next = format!("[{}]", msgs.join(","));
            let prev_chunks = chunk_value_from(prev.as_bytes(), 0).unwrap();
            let whole = chunk_value_from(next.as_bytes(), 0).unwrap();
            // The last chunk of `prev` that ends at a cut inside the shared prefix.
            let lcp = prev
                .bytes()
                .zip(next.bytes())
                .take_while(|(a, b)| a == b)
                .count();
            let k = prev_chunks
                .iter()
                .rposition(|c| c.at_cut && c.range.end <= lcp)
                .unwrap();
            let from = prev_chunks[k].range.end;
            let mut reused: Vec<Chunk> = prev_chunks[..=k].to_vec();
            reused.extend(chunk_value_from(next.as_bytes(), from).unwrap());
            assert_eq!(reused, whole, "split {split}");
        }
        // A rest that is not the rest of a JSON array.
        assert_eq!(chunk_value_from(b"[1,2 x", 3), None);
        assert_eq!(chunk_value_from(b"[1,2]", 5), Some(Vec::new()));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(500))]
        #[test]
        fn gear_pieces_match_reference(data in gear_input()) {
            let mut out = Vec::new();
            gear_pieces(&data, &mut out, 0);
            let mut slices = Vec::new();
            RefGear::new().chunks(&data, &mut slices);
            let got: Vec<usize> = out.iter().map(|r| r.len()).collect();
            let want: Vec<usize> = slices.iter().map(|s| s.len()).collect();
            prop_assert_eq!(got, want);
        }
    }

    #[test]
    fn json_units_fixed_cases() {
        assert_eq!(json_units(b"[]"), Some(vec![]));
        assert_eq!(json_units(b"[1]"), Some(vec![]));
        assert_eq!(json_units(b"[1,2]"), Some(vec![3]));
        assert_eq!(json_units(b"  [1, 2]  "), Some(vec![5]));
        assert_eq!(json_units(b"[[1,2],[3]]"), Some(vec![7]));
        assert_eq!(
            json_units(br#"["a,b","c]d","e\"f",{"k":[1,2]}]"#),
            Some(vec![7, 13, 20])
        );
        assert_eq!(json_units(br#"["\\",1]"#), Some(vec![6]));
        assert_eq!(json_units(b"[1,2"), None);
        assert_eq!(json_units(b"[1]]"), None);
        assert_eq!(json_units(b"[1] x"), None);
        assert_eq!(json_units(b"[1}"), None);
        assert_eq!(json_units(br#"{"a":1}"#), None);
        assert_eq!(json_units(b"\"[1,2]\""), None);
        assert_eq!(json_units(b"["), None);
        assert_eq!(json_units(br#"["unterminated]"#), None);
        assert_eq!(json_units(b"[\"a\\"), None);
        assert_eq!(json_units(b""), None);
        assert_eq!(json_units(b"   "), None);
        assert_eq!(json_units(b"[1]\x0c"), None);
    }

    #[test]
    fn chunk_value_empty_and_short_non_json() {
        assert!(chunk_value(b"").is_empty());
        assert_eq!(chunk_value(b"x"), vec![0..1]);
        assert_eq!(chunk_value(&b"x".repeat(255)), vec![0..255]);
        assert_eq!(chunk_value(&b"x".repeat(256)), vec![0..256]);
    }

    #[test]
    fn chunk_value_four_short_elements() {
        let s = "x".repeat(100);
        let value = serde_json::to_string(&vec![s.clone(), s.clone(), s.clone(), s]).unwrap();
        assert_eq!(value.len(), 413);
        assert_eq!(chunk_value(value.as_bytes()), vec![0..310, 310..413]);
    }

    fn xorshift_alnum(len: usize, seed: u64) -> String {
        const ALNUM: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let mut x = seed;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                char::from(ALNUM[(x % 62) as usize])
            })
            .collect()
    }

    #[test]
    fn chunk_value_one_large_element() {
        let big = xorshift_alnum(102_400, 0x9e37_79b9_7f4a_7c15);
        let value = serde_json::to_string(&vec!["a".to_string(), big, "b".to_string()]).unwrap();
        let chunks = chunk_value(value.as_bytes());
        assert!(chunks.len() >= 13);
        for c in chunks.iter().rev().skip(1) {
            assert!(c.len() >= MIN_CHUNK);
            assert!(c.len() <= GEAR_MAX + MIN_CHUNK);
        }
        assert_eq!(join(value.as_bytes(), &chunks), value.as_bytes());
    }

    #[test]
    fn chunk_value_invalid_json_matches_gear_accumulation() {
        let mut value = b"[1,2".to_vec();
        value.extend(vec![b'x'; 20_000]);
        assert_eq!(json_units(&value), None);
        assert_eq!(chunk_value(&value), ref_chunk_value(&value));
    }

    fn check_chunk_invariants(value: &[u8]) -> Result<(), TestCaseError> {
        let chunks = chunk_value(value);
        let mut pos = 0usize;
        for c in &chunks {
            prop_assert_eq!(c.start, pos);
            prop_assert!(c.start < c.end);
            pos = c.end;
        }
        prop_assert_eq!(pos, value.len());
        prop_assert_eq!(join(value, &chunks), value.to_vec());
        for c in chunks.iter().rev().skip(1) {
            prop_assert!(c.len() >= MIN_CHUNK);
        }
        prop_assert_eq!(chunk_value(value), chunks.clone());
        // The chunks of UTF-8 text are UTF-8: no cut falls inside a character.
        if std::str::from_utf8(value).is_ok() {
            for c in &chunks {
                prop_assert!(std::str::from_utf8(&value[c.clone()]).is_ok(), "{:?}", c);
            }
        }
        if let Some(cuts) = json_units(value) {
            let mut starts = vec![0usize];
            starts.extend(cuts.iter().copied());
            let mut ends = cuts.clone();
            ends.push(value.len());
            let all_small = starts
                .iter()
                .zip(&ends)
                .all(|(s, e)| e - s < GEAR_UNIT_THRESHOLD);
            if all_small {
                for c in chunks.iter().rev().skip(1) {
                    prop_assert!(cuts.contains(&c.end));
                }
            }
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(500))]
        #[test]
        fn chunk_value_invariants(value in value_strategy()) {
            check_chunk_invariants(&value)?;
        }
    }
}
