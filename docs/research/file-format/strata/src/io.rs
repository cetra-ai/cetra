//! Coalescing of segment reads and the I/O counters (design S7 § I/O).
//!
//! Every segment read goes through [`StrataFile::fetch`](crate::file::StrataFile), which
//! merges nearby byte ranges according to the scan kind and fetches them as one wave.

use std::ops::Range;

/// How reads are coalesced (design S7 § I/O).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanKind {
    /// Point lookups: merge ranges whose gap is below 4 KiB.
    Point,
    /// Scans: merge ranges whose gap is below 1 MiB, without growing a merged range past 16 MiB.
    Scan,
}

/// I/O counters of one `StrataFile` since it was opened (or since `reset_io_stats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IoStats {
    /// Ranged GET requests issued.
    pub requests: u64,
    /// Bytes fetched by those requests.
    pub bytes: u64,
    /// Waves: sets of requests issued together, each waiting on the previous one.
    pub waves: u64,
    /// Segment bytes fetched, by segment kind (index: the kind number of the design's segment
    /// table); gaps merged into requests are not counted.
    pub by_kind: [u64; 10],
}

/// Largest gap merged for [`ScanKind::Point`].
pub(crate) const POINT_GAP: u64 = 4 * 1024;
/// Largest gap merged for [`ScanKind::Scan`].
pub(crate) const SCAN_GAP: u64 = 1024 * 1024;
/// Largest merged range for [`ScanKind::Scan`], unless a single input is larger.
pub(crate) const SCAN_MAX_RANGE: u64 = 16 * 1024 * 1024;

/// Most ranged GET requests of one wave that are in flight at once.
pub(crate) const MAX_CONCURRENT_GETS: usize = 64;

/// Merged byte ranges covering `ranges`, plus for each input the index of the merged range
/// holding it. Inputs may overlap or repeat; output ranges are sorted and disjoint.
pub(crate) fn coalesce(ranges: &[Range<u64>], kind: ScanKind) -> (Vec<Range<u64>>, Vec<usize>) {
    let gap = match kind {
        ScanKind::Point => POINT_GAP,
        ScanKind::Scan => SCAN_GAP,
    };
    let mut order: Vec<(u64, u64, usize)> = ranges
        .iter()
        .enumerate()
        .map(|(i, r)| (r.start, r.end, i))
        .collect();
    order.sort_unstable();

    let mut merged: Vec<Range<u64>> = Vec::new();
    let mut owner = vec![0usize; ranges.len()];
    for (a, b, i) in order {
        let extend = match merged.last() {
            // `a >= e` below, so `a - e` does not underflow.
            Some(cur) => {
                let (s, e) = (cur.start, cur.end);
                a < e
                    || (a - e <= gap
                        && (kind == ScanKind::Point
                            || b.max(e).saturating_sub(s) <= SCAN_MAX_RANGE))
            }
            None => false,
        };
        if extend {
            if let Some(cur) = merged.last_mut() {
                cur.end = cur.end.max(b);
            }
        } else {
            merged.push(a..b);
        }
        if let Some(slot) = owner.get_mut(i) {
            *slot = merged.len().saturating_sub(1);
        }
    }
    (merged, owner)
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

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn point_merges_small_gaps_only() {
        let (merged, owner) = coalesce(&[0..10, 100..110], ScanKind::Point);
        assert_eq!(merged, vec![0..110]);
        assert_eq!(owner, vec![0, 0]);

        let (merged, _) = coalesce(&[0..10, 70_000..70_010], ScanKind::Point);
        assert_eq!(merged, vec![0..10, 70_000..70_010]);
    }

    #[test]
    fn scan_merges_up_to_the_gap_and_the_cap() {
        let (merged, _) = coalesce(&[0..10, 500_000..500_010], ScanKind::Scan);
        assert_eq!(merged, vec![0..500_010]);

        let (merged, _) = coalesce(
            &[0..10 * MIB, (10 * MIB + 1)..(20 * MIB + 1)],
            ScanKind::Scan,
        );
        assert_eq!(merged.len(), 2);

        let (merged, _) = coalesce(std::slice::from_ref(&(0..20 * MIB)), ScanKind::Scan);
        assert_eq!(merged, vec![0..20 * MIB]);

        let (merged, owner) = coalesce(&[0..20 * MIB, 5..10], ScanKind::Scan);
        assert_eq!(merged, vec![0..20 * MIB]);
        assert_eq!(owner, vec![0, 0]);
    }

    #[test]
    fn repeated_inputs_share_one_range() {
        let (merged, owner) = coalesce(&[5..9, 5..9], ScanKind::Point);
        assert_eq!(merged, vec![5..9]);
        assert_eq!(owner, vec![0, 0]);
    }

    #[test]
    fn empty_input_gives_empty_outputs() {
        let (merged, owner) = coalesce(&[], ScanKind::Scan);
        assert!(merged.is_empty());
        assert!(owner.is_empty());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(300))]
        #[test]
        fn coalesce_invariants_hold(
            raw in proptest::collection::vec((0u64..(64 * MIB), 0u64..(8 * MIB)), 0..=20)
        ) {
            let ranges: Vec<Range<u64>> = raw.iter().map(|&(s, l)| s..s + l).collect();
            let disjoint = ranges.iter().enumerate().all(|(i, a)| {
                ranges
                    .iter()
                    .enumerate()
                    .all(|(j, b)| i == j || a.end <= b.start || b.end <= a.start)
            });
            for kind in [ScanKind::Point, ScanKind::Scan] {
                let (merged, owner) = coalesce(&ranges, kind);
                prop_assert_eq!(owner.len(), ranges.len());
                for (r, &m) in ranges.iter().zip(&owner) {
                    let outer = merged.get(m).expect("owner index in range");
                    prop_assert!(outer.start <= r.start && r.end <= outer.end);
                }
                for pair in merged.windows(2) {
                    prop_assert!(pair[0].end <= pair[1].start);
                }
                if kind == ScanKind::Scan && disjoint {
                    for m in &merged {
                        if m.end - m.start > SCAN_MAX_RANGE {
                            prop_assert!(ranges.iter().any(|r| r == m));
                        }
                    }
                }
            }
        }
    }
}
