//! Refs pages (S4 step 4). Each row of a page holds the list of chunk ids of its value. A
//! row whose list starts with the list of a recent row is stored as the new ids on top of
//! that row (a based record), so consecutive values that share a prefix cost little.
//!
//! Page layout: `u32 n_rows`, `u32 row_offset[n_rows + 1]` (relative to the record area,
//! `row_offset[0] = 0`), then the records. A record is a tag byte and its fields:
//! - `0` null, no further bytes;
//! - `1` keyframe: `uvarint n`, then the ids (the first as a `uvarint`, each later one as the
//!   zigzag of its difference from the previous id);
//! - `2` based: `uvarint back`, `uvarint n_new`, then the new ids (the first as the zigzag of
//!   its difference from the base's last id, the others as above).

use super::varint::{get_uvarint, put_uvarint, unzigzag, zigzag};
use super::{MAX_PAGE_IDS, REFS_MAX_BACK, REFS_MAX_DEPTH};
use crate::error::{Result, StrataError};
use crate::format::Cursor;

const TAG_NULL: u8 = 0;
const TAG_KEYFRAME: u8 = 1;
const TAG_BASED: u8 = 2;

/// One row to encode.
pub(crate) enum RowRefs<'a> {
    /// A null value.
    Null,
    /// The chunk ids of a value. Empty for an empty value.
    Ids(&'a [u32]),
}

/// What the encoder keeps of an earlier row: its full list and its depth. `None` for a null
/// row.
type Entry = Option<(Vec<u32>, u8)>;

/// Encodes a page, looking at most `max_back` rows back. Returns, for each row, `Some((back,
/// depth))` when the row is a based record and `None` otherwise.
pub(crate) fn encode_page_with(
    rows: &[RowRefs<'_>],
    max_back: usize,
    out: &mut Vec<u8>,
) -> Result<Vec<Option<(usize, u8)>>> {
    let too_big = || StrataError::Unsupported {
        what: "a refs page of more than 4 GiB".to_string(),
    };
    let n_rows = u32::try_from(rows.len()).map_err(|_| too_big())?;
    let mut records: Vec<u8> = Vec::new();
    let mut row_offsets: Vec<u32> = Vec::with_capacity(rows.len() + 1);
    row_offsets.push(0);
    let mut entries: Vec<Entry> = Vec::with_capacity(rows.len());
    let mut based = Vec::with_capacity(rows.len());

    for (r, row) in rows.iter().enumerate() {
        match row {
            RowRefs::Null => {
                records.push(TAG_NULL);
                entries.push(None);
                based.push(None);
            }
            RowRefs::Ids(ids) => {
                let ids: &[u32] = ids;
                match choose_base(ids, r, max_back, &entries) {
                    Some((back, base_depth)) => {
                        let base: &[u32] = entries
                            .get(r - back)
                            .and_then(|e| e.as_ref())
                            .map(|(list, _)| list.as_slice())
                            .unwrap_or_default();
                        let new_ids = ids.get(base.len()..).unwrap_or_default();
                        records.push(TAG_BASED);
                        put_uvarint(&mut records, back as u64);
                        put_uvarint(&mut records, new_ids.len() as u64);
                        put_ids(&mut records, base.last().copied(), new_ids);
                        let mut full = base.to_vec();
                        full.extend_from_slice(new_ids);
                        let depth = base_depth + 1;
                        entries.push(Some((full, depth)));
                        based.push(Some((back, depth)));
                    }
                    None => {
                        records.push(TAG_KEYFRAME);
                        put_uvarint(&mut records, ids.len() as u64);
                        put_ids(&mut records, None, ids);
                        entries.push(Some((ids.to_vec(), 0)));
                        based.push(None);
                    }
                }
            }
        }
        let end = u32::try_from(records.len()).map_err(|_| too_big())?;
        row_offsets.push(end);
    }

    out.extend_from_slice(&n_rows.to_le_bytes());
    for offset in &row_offsets {
        out.extend_from_slice(&offset.to_le_bytes());
    }
    out.extend_from_slice(&records);
    Ok(based)
}

/// Encodes a page with the default look-back.
pub(crate) fn encode_page(rows: &[RowRefs<'_>], out: &mut Vec<u8>) -> Result<()> {
    encode_page_with(rows, REFS_MAX_BACK, out).map(|_| ())
}

/// The base for a row with `ids`: among the rows `r - k` for `k` in `1..=max_back`, the
/// non-null, non-empty ones whose list is a prefix of `ids` and whose depth leaves room for one
/// more level. The longest base wins, and on a tie the nearest. Returns `(back, base depth)`.
fn choose_base(ids: &[u32], r: usize, max_back: usize, entries: &[Entry]) -> Option<(usize, u8)> {
    if ids.is_empty() {
        return None;
    }
    let mut best: Option<(usize, u8, usize)> = None;
    for back in 1..=max_back.min(r) {
        let Some(Some((base, depth))) = entries.get(r - back) else {
            continue;
        };
        if base.is_empty() || *depth >= REFS_MAX_DEPTH || !ids.starts_with(base) {
            continue;
        }
        if best.is_none_or(|(_, _, len)| base.len() > len) {
            best = Some((back, *depth, base.len()));
        }
    }
    best.map(|(back, depth, _)| (back, depth))
}

/// Writes `ids` as deltas. The first id is absolute when `prev` is `None`, and the zigzag
/// difference from `prev` otherwise. Each later id is the difference from the one before.
fn put_ids(out: &mut Vec<u8>, prev: Option<u32>, ids: &[u32]) {
    let mut last: Option<i64> = prev.map(i64::from);
    for &id in ids {
        let value = i64::from(id);
        match last {
            None => put_uvarint(out, u64::from(id)),
            Some(p) => put_uvarint(out, zigzag(value - p)),
        }
        last = Some(value);
    }
}

/// Reads `n` ids written by [`put_ids`]. The count is checked against the remaining bytes
/// before the call. Every id must fit in `u32`.
fn read_ids(c: &mut Cursor<'_>, n: usize, prev: Option<u32>, offset: u64) -> Result<Vec<u32>> {
    let corrupt = || StrataError::Corrupt {
        what: "refs id",
        offset,
    };
    let mut ids = Vec::with_capacity(n);
    let mut last: Option<i64> = prev.map(i64::from);
    for _ in 0..n {
        let value = match last {
            None => {
                let v = get_uvarint(c, "refs id")?;
                i64::try_from(v).map_err(|_| corrupt())?
            }
            Some(p) => p
                .checked_add(unzigzag(get_uvarint(c, "refs id")?))
                .ok_or_else(corrupt)?,
        };
        let id = u32::try_from(value).map_err(|_| corrupt())?;
        ids.push(id);
        last = Some(value);
    }
    Ok(ids)
}

/// Reads a count and checks it against the remaining bytes: every id takes at least one byte.
fn count(c: &mut Cursor<'_>, what: &'static str, offset: u64) -> Result<usize> {
    let v = get_uvarint(c, what)?;
    let remaining = c.remaining();
    usize::try_from(v)
        .ok()
        .filter(|&n| n <= remaining)
        .ok_or(StrataError::Corrupt { what, offset })
}

/// Adds `extra` chunk ids to the running total of a page, and fails past `MAX_PAGE_IDS`.
fn add_page_ids(total: usize, extra: usize, offset: u64) -> Result<usize> {
    total
        .checked_add(extra)
        .filter(|sum| *sum <= MAX_PAGE_IDS)
        .ok_or(StrataError::Corrupt {
            what: "refs ids",
            offset,
        })
}

/// Decodes a refs page: one entry per row, the row's full list of chunk ids, or `None` for a
/// null row. `offset` is the page's file offset, used in errors.
pub(crate) fn decode_page(bytes: &[u8], offset: u64) -> Result<Vec<Option<Vec<u32>>>> {
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset };
    let mut c = Cursor::new(bytes, offset);
    let n_rows = c.u32("refs rows")? as usize;
    // The offset table has n_rows + 1 entries. Checking its size bounds n_rows before
    // anything is allocated.
    let table_len = n_rows
        .checked_add(1)
        .and_then(|v| v.checked_mul(4))
        .ok_or_else(|| corrupt("refs rows"))?;
    if table_len > c.remaining() {
        return Err(corrupt("refs rows"));
    }
    let mut row_offsets = Vec::with_capacity(n_rows + 1);
    for _ in 0..=n_rows {
        row_offsets.push(c.u32("refs offset")? as usize);
    }
    if row_offsets.first() != Some(&0)
        || row_offsets.windows(2).any(|w| matches!(w, [a, b] if a > b))
    {
        return Err(corrupt("refs offset"));
    }
    let rest = c.remaining();
    let records = c.bytes(rest, "refs records")?;
    if row_offsets.last().copied() != Some(records.len()) {
        return Err(corrupt("refs offset"));
    }

    let mut lists: Vec<Option<Vec<u32>>> = Vec::with_capacity(n_rows);
    let mut depths: Vec<u8> = Vec::with_capacity(n_rows);
    // The chunk ids of the rows decoded so far, with tail sharing expanded.
    let mut total_ids = 0usize;
    for (r, window) in row_offsets.windows(2).enumerate() {
        let [start, end] = window else {
            return Err(corrupt("refs offset"));
        };
        let record = records
            .get(*start..*end)
            .ok_or_else(|| corrupt("refs record"))?;
        let mut rc = Cursor::new(record, offset);
        let tag = rc.u8("refs tag")?;
        let (list, depth) = match tag {
            TAG_NULL => (None, 0),
            TAG_KEYFRAME => {
                let n = count(&mut rc, "refs keyframe", offset)?;
                total_ids = add_page_ids(total_ids, n, offset)?;
                (Some(read_ids(&mut rc, n, None, offset)?), 0)
            }
            TAG_BASED => {
                let back = get_uvarint(&mut rc, "refs back")?;
                let back = usize::try_from(back)
                    .ok()
                    .filter(|&b| (1..=REFS_MAX_BACK).contains(&b) && b <= r)
                    .ok_or_else(|| corrupt("refs back"))?;
                let base_index = r - back;
                let base = lists
                    .get(base_index)
                    .and_then(|list| list.as_ref())
                    .filter(|list| !list.is_empty())
                    .ok_or_else(|| corrupt("refs base"))?;
                let base_depth = depths.get(base_index).copied().unwrap_or(0);
                let depth = base_depth
                    .checked_add(1)
                    .filter(|d| *d <= REFS_MAX_DEPTH)
                    .ok_or_else(|| corrupt("refs depth"))?;
                let n_new = count(&mut rc, "refs new ids", offset)?;
                total_ids = add_page_ids(total_ids, base.len().saturating_add(n_new), offset)?;
                let new_ids = read_ids(&mut rc, n_new, base.last().copied(), offset)?;
                let mut full = base.clone();
                full.extend_from_slice(&new_ids);
                (Some(full), depth)
            }
            _ => return Err(corrupt("refs tag")),
        };
        if rc.remaining() != 0 {
            return Err(corrupt("refs record length"));
        }
        lists.push(list);
        depths.push(depth);
    }
    Ok(lists)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A page from raw records, built by hand.
    fn page(records: &[Vec<u8>]) -> Vec<u8> {
        let mut out = (records.len() as u32).to_le_bytes().to_vec();
        let mut end = 0u32;
        out.extend_from_slice(&end.to_le_bytes());
        for record in records {
            end += record.len() as u32;
            out.extend_from_slice(&end.to_le_bytes());
        }
        for record in records {
            out.extend_from_slice(record);
        }
        out
    }

    fn is_corrupt<T: std::fmt::Debug>(result: Result<T>) -> bool {
        matches!(result, Err(StrataError::Corrupt { .. }))
    }

    /// The id lists of the id-cap tests: group `g` is `g × 16_384 .. (g + 1) × 16_384`, and row
    /// `r` of a page of `rows` rows holds group `r / 16`.
    fn cap_groups(rows: usize) -> Vec<Vec<u32>> {
        (0..rows.div_ceil(16))
            .map(|g| {
                let start = (g * 16_384) as u32;
                (start..start + 16_384).collect()
            })
            .collect()
    }

    #[test]
    fn page_with_more_than_2_pow_24_expanded_ids_is_corrupt() {
        // 4,096 rows of 16,384 ids each: 2^26 ids once expanded, which is over the cap.
        let groups = cap_groups(4_096);
        let refs: Vec<RowRefs<'_>> = (0..4_096)
            .map(|r| RowRefs::Ids(groups[r / 16].as_slice()))
            .collect();
        let mut bytes = Vec::new();
        encode_page(&refs, &mut bytes).unwrap();
        assert!(matches!(
            decode_page(&bytes, 0),
            Err(StrataError::Corrupt {
                what: "refs ids",
                ..
            })
        ));
    }

    #[test]
    fn page_at_exactly_2_pow_24_expanded_ids_decodes() {
        // 1,024 rows of 16,384 ids each: exactly 2^24 ids.
        let groups = cap_groups(1_024);
        let rows: Vec<Option<Vec<u32>>> =
            (0..1_024).map(|r| Some(groups[r / 16].clone())).collect();
        let refs: Vec<RowRefs<'_>> = rows.iter().map(to_refs).collect();
        let mut bytes = Vec::new();
        encode_page(&refs, &mut bytes).unwrap();
        assert_eq!(decode_page(&bytes, 0).unwrap(), rows);
    }

    /// Row 0 is the keyframe [7]; each later row adds the id 1 more, based on the row before.
    fn chain(rows: usize) -> Vec<Vec<u8>> {
        let mut records = vec![vec![TAG_KEYFRAME, 1, 7]];
        for _ in 1..rows {
            records.push(vec![TAG_BASED, 1, 1, zigzag(1) as u8]);
        }
        records
    }

    fn to_refs(row: &Option<Vec<u32>>) -> RowRefs<'_> {
        match row {
            None => RowRefs::Null,
            Some(ids) => RowRefs::Ids(ids.as_slice()),
        }
    }

    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn spec() -> impl Strategy<Value = Vec<(u8, usize, Vec<u32>, Vec<u32>)>> {
        prop::collection::vec(
            (
                0u8..100,
                any::<usize>(),
                prop::collection::vec(any::<u32>(), 0..=5),
                prop::collection::vec(any::<u32>(), 1..20),
            ),
            1..600,
        )
    }

    /// Rows from a spec: null 10%, empty 5%, a prefix of the last non-empty row plus up to five
    /// ids 60% (a fresh list when there is no such row), a fresh list 25%.
    fn rows_from(spec: &[(u8, usize, Vec<u32>, Vec<u32>)]) -> Vec<Option<Vec<u32>>> {
        let mut rows: Vec<Option<Vec<u32>>> = Vec::with_capacity(spec.len());
        let mut last: Option<Vec<u32>> = None;
        for (kind, cut, extra, fresh) in spec {
            let row = match *kind {
                0..=9 => None,
                10..=14 => Some(Vec::new()),
                15..=74 => match &last {
                    Some(prev) => {
                        let keep = 1 + cut % prev.len();
                        let mut list = prev[..keep].to_vec();
                        list.extend_from_slice(extra);
                        Some(list)
                    }
                    None => Some(fresh.clone()),
                },
                _ => Some(fresh.clone()),
            };
            if let Some(list) = row.as_ref().filter(|list| !list.is_empty()) {
                last = Some(list.clone());
            }
            rows.push(row);
        }
        rows
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(300))]

        #[test]
        fn pages_round_trip(spec in spec()) {
            let rows = rows_from(&spec);
            let refs: Vec<RowRefs<'_>> = rows.iter().map(to_refs).collect();
            let mut bytes = Vec::new();
            encode_page(&refs, &mut bytes).unwrap();
            let decoded = decode_page(&bytes, 0).unwrap();
            prop_assert_eq!(&decoded, &rows);

            let mut scratch = Vec::new();
            let based = encode_page_with(&refs, REFS_MAX_BACK, &mut scratch).unwrap();
            for (back, depth) in based.iter().flatten() {
                prop_assert!(*back <= REFS_MAX_BACK);
                prop_assert!(*depth <= REFS_MAX_DEPTH);
            }
        }

        #[test]
        fn random_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
            let _ = decode_page(&bytes, 0);
        }
    }

    #[test]
    fn conversation_page_is_much_smaller_than_keyframes() {
        // Row k is row k - 1 plus two new random ids, for 100 rows.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut list: Vec<u32> = Vec::new();
        let mut lists = Vec::new();
        for _ in 0..100 {
            for _ in 0..2 {
                list.push(xorshift(&mut state) as u32);
            }
            lists.push(list.clone());
        }
        let rows: Vec<RowRefs<'_>> = lists.iter().map(|l| RowRefs::Ids(l.as_slice())).collect();
        let mut based = Vec::new();
        encode_page_with(&rows, REFS_MAX_BACK, &mut based).unwrap();
        let mut keyframes = Vec::new();
        encode_page_with(&rows, 0, &mut keyframes).unwrap();
        assert!(
            based.len() * 100 < keyframes.len() * 35,
            "{} vs {}",
            based.len(),
            keyframes.len()
        );
        assert_eq!(
            decode_page(&based, 0).unwrap(),
            lists.into_iter().map(Some).collect::<Vec<_>>()
        );
    }

    #[test]
    fn based_record_with_an_empty_base_is_corrupt() {
        let bytes = page(&[vec![TAG_KEYFRAME, 0], vec![TAG_BASED, 1, 1, 10]]);
        assert!(is_corrupt(decode_page(&bytes, 0)));
    }

    #[test]
    fn based_record_reaching_past_the_first_row_is_corrupt() {
        let bytes = page(&[vec![TAG_KEYFRAME, 1, 5], vec![TAG_BASED, 2, 0]]);
        assert!(is_corrupt(decode_page(&bytes, 0)));
    }

    #[test]
    fn back_of_zero_is_corrupt() {
        let bytes = page(&[vec![TAG_KEYFRAME, 1, 5], vec![TAG_BASED, 0, 0]]);
        assert!(is_corrupt(decode_page(&bytes, 0)));
    }

    #[test]
    fn depth_sixteen_is_corrupt() {
        // Sixteen rows form a chain of depths 0 to 15, which is allowed.
        assert!(decode_page(&page(&chain(16)), 0).is_ok());
        // A seventeenth row would have depth 16.
        assert!(is_corrupt(decode_page(&page(&chain(17)), 0)));
    }

    #[test]
    fn unknown_tag_and_trailing_bytes_are_corrupt() {
        assert!(is_corrupt(decode_page(&page(&[vec![9]]), 0)));
        assert!(is_corrupt(decode_page(&page(&[vec![TAG_NULL, 0]]), 0)));
    }

    #[test]
    fn null_and_empty_rows_round_trip() {
        let rows = vec![None, Some(Vec::new()), Some(vec![4, 5])];
        let refs: Vec<RowRefs<'_>> = rows.iter().map(to_refs).collect();
        let mut bytes = Vec::new();
        encode_page(&refs, &mut bytes).unwrap();
        assert_eq!(decode_page(&bytes, 0).unwrap(), rows);
    }
}
