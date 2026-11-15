//! Value chunking (S4 steps 1-5): cuts one payload value into chunks.
//!
//! A JSON array is first split into its top-level elements. Long elements are cut
//! by a gear hash, short ones stay whole. Adjacent pieces are then merged until each
//! chunk is at least `MIN_CHUNK` bytes, except the last.

use std::ops::Range;

/// Minimum size of every chunk of a value except its last (S4 step 5).
pub(crate) const MIN_CHUNK: usize = 256;
/// A JSON unit at least this long is cut by the gear chunker (S4 step 2).
pub(crate) const GEAR_UNIT_THRESHOLD: usize = 8192;
/// Gear chunker parameters (S4 step 2).
pub(crate) const GEAR_MIN: usize = 512;
pub(crate) const GEAR_MAX: usize = 8192;
pub(crate) const GEAR_MASK: u64 = (1 << 11) - 1;
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
        out.push(base + start..base + start + cut);
        start += cut;
    }
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// Returns the cut positions of a JSON array: the absolute indexes where a new
/// top-level element starts. `None` when `value` is not a JSON array the scanner
/// accepts.
fn json_units(value: &[u8]) -> Option<Vec<usize>> {
    let first = value.iter().position(|&b| !is_ws(b))?;
    if value.get(first) != Some(&b'[') {
        return None;
    }
    let (mut depth, mut in_string, mut escape) = (1u32, false, false);
    let mut cuts = Vec::new();
    for (i, &c) in value.iter().enumerate().skip(first + 1) {
        if in_string {
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                depth -= 1; // depth ≥ 1 here, so no underflow
                if depth == 0 {
                    let tail_ok = value
                        .get(i + 1..)
                        .is_some_and(|t| t.iter().all(|&b| is_ws(b)));
                    return (c == b']' && tail_ok).then_some(cuts);
                }
            }
            b',' if depth == 1 => cuts.push(i + 1),
            _ => {}
        }
    }
    None // loop ended with depth > 0 or inside a string
}

/// Cuts `value` into chunk ranges. The ranges are contiguous, non-empty, cover
/// `0..value.len()`, and every range but the last is at least `MIN_CHUNK` bytes.
pub(crate) fn chunk_value(value: &[u8]) -> Vec<Range<usize>> {
    if value.is_empty() {
        return Vec::new();
    }
    let mut pieces: Vec<Range<usize>> = Vec::new();
    match json_units(value) {
        Some(cuts) => {
            let mut unit_start = 0usize;
            for end in cuts.iter().copied().chain(std::iter::once(value.len())) {
                let unit = unit_start..end;
                if unit.len() < GEAR_UNIT_THRESHOLD {
                    pieces.push(unit.clone());
                } else {
                    let data = value.get(unit.clone()).unwrap_or_default();
                    gear_pieces(data, &mut pieces, unit.start);
                }
                unit_start = end;
            }
        }
        None => gear_pieces(value, &mut pieces, 0),
    }

    let mut out = Vec::new();
    let mut chunk_start = 0usize;
    for p in &pieces {
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

    /// Copy of `bench/src/payload.rs::Gear`, the reference chunker.
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
            const MIN: usize = 512;
            const MAX: usize = 8192;
            const MASK: u64 = (1 << 11) - 1;
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
            (vec![0; 512], vec![512]),
            (vec![0; 513], vec![513]),
            (vec![0; 8192], vec![8192]),
            (vec![0; 8193], vec![8192, 1]),
            (vec![0; 9000], vec![8192, 808]),
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
    fn gear_pieces_base_shifts_ranges() {
        let data = vec![0u8; 9000];
        let mut out = Vec::new();
        gear_pieces(&data, &mut out, 1000);
        assert_eq!(out, vec![1000..9192, 9192..10000]);
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
            assert!(c.len() <= GEAR_MAX + 255);
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
