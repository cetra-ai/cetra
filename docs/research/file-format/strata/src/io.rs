//! Coalescing of segment reads and the I/O counters (design S7 § I/O).
//!
//! Every segment read goes through [`StrataFile::fetch`](crate::file::StrataFile), which
//! merges nearby byte ranges according to the scan kind and fetches them as one wave.

use std::ops::Range;

use object_store::path::Path;
use object_store::{GetOptions, GetRange, ObjectStore, ObjectStoreExt};
use tokio::sync::Semaphore;

use crate::error::Result;

/// How reads are coalesced (design S7 § I/O).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanKind {
    /// Point lookups and metadata: merge ranges whose gap is at most `Config::point_gap`
    /// (default 4 KiB) into ranges of at most `Config::point_max_range` (default unbounded).
    Point,
    /// Scans: merge ranges whose gap is below 1 MiB, without growing a merged range past 4 MiB.
    Scan,
    /// Many scattered segments, such as the candidate frames of a common needle: merge ranges
    /// whose gap is below 128 KiB, without growing a merged range past 4 MiB. On S3 a request
    /// costs about as much as 150 KB on the link when 256 are in flight (30 ms each, 10 Gbit/s),
    /// so a larger gap reads more than it saves and a smaller one makes more requests.
    Dense,
    /// Scattered segments of a search: index pages, and candidate frames that fill under a
    /// quarter of their span. Merge ranges whose gap is below 4 KiB on any store: on R2 a 256 KiB
    /// gap made searches up to 2.8 times slower, as the merged gaps outweigh the saved requests
    /// (`real-data.md` § Real S3).
    Sparse,
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

/// Default largest gap merged for [`ScanKind::Point`] (`Config::point_gap`).
pub(crate) const POINT_GAP: u64 = 4 * 1024;
/// Largest gap merged for [`ScanKind::Dense`].
pub(crate) const DENSE_GAP: u64 = 128 * 1024;
/// Largest gap merged for [`ScanKind::Scan`].
pub(crate) const SCAN_GAP: u64 = 1024 * 1024;
/// Largest merged range for [`ScanKind::Scan`] and [`ScanKind::Dense`], unless a single input is
/// larger. One S3 request runs at one connection's throughput (about 90 MB/s), so a larger range
/// is slower than several fetched at once: 4 MiB takes about as long to transfer as a request's
/// first-byte latency (`real-data.md` § Simulated S3).
pub(crate) const SCAN_MAX_RANGE: u64 = 4 * 1024 * 1024;

/// Most ranged GET requests in flight in the process, across every file, wave and query. A
/// local store opens a file descriptor for each request, so this also bounds the descriptors
/// reads hold: a search over 20 files once issued 2,560 at a time and passed the default soft
/// limit of 1,024 (`real-data.md` § Braintrust's search benchmark).
pub(crate) const MAX_GETS_IN_FLIGHT: usize = 256;

static GETS_IN_FLIGHT: Semaphore = Semaphore::const_new(MAX_GETS_IN_FLIGHT);

/// Fetches `range` of `path`, holding one of the process's [`MAX_GETS_IN_FLIGHT`] permits
/// while the request runs. Every ranged read of a file goes through here.
pub(crate) async fn get_range(
    store: &dyn ObjectStore,
    path: &Path,
    range: Range<u64>,
) -> Result<bytes::Bytes> {
    // `acquire` fails only on a closed semaphore, and this one is never closed.
    let _permit = GETS_IN_FLIGHT.acquire().await.ok();
    Ok(store.get_range(path, range).await?)
}

/// Fetches the last `n` bytes of `path` (all of it if shorter) under the same limit. Returns
/// the object's size, the offset of the first byte returned, and the bytes.
pub(crate) async fn get_suffix(
    store: &dyn ObjectStore,
    path: &Path,
    n: u64,
) -> Result<(u64, u64, bytes::Bytes)> {
    let _permit = GETS_IN_FLIGHT.acquire().await.ok();
    let read = store
        .get_opts(
            path,
            GetOptions {
                range: Some(GetRange::Suffix(n)),
                ..Default::default()
            },
        )
        .await?;
    let (size, start) = (read.meta.size, read.range.start);
    Ok((size, start, read.bytes().await?))
}

/// Merged byte ranges covering `ranges`, plus for each input the index of the merged range
/// holding it. [`ScanKind::Point`] merges gaps up to `point.0` into ranges up to `point.1`
/// (unless one input is larger). Inputs may overlap or repeat; output ranges are sorted and
/// disjoint.
pub(crate) fn coalesce(
    ranges: &[Range<u64>],
    kind: ScanKind,
    point: (u64, u64),
) -> (Vec<Range<u64>>, Vec<usize>) {
    let (point_gap, point_max) = point;
    let gap = match kind {
        ScanKind::Point => point_gap,
        ScanKind::Sparse => POINT_GAP,
        ScanKind::Scan => SCAN_GAP,
        ScanKind::Dense => DENSE_GAP,
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
                let max = match kind {
                    ScanKind::Point => point_max,
                    ScanKind::Sparse => u64::MAX,
                    ScanKind::Scan | ScanKind::Dense => SCAN_MAX_RANGE,
                };
                a < e || (a - e <= gap && b.max(e).saturating_sub(s) <= max)
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
        let (merged, owner) = coalesce(&[0..10, 100..110], ScanKind::Point, (POINT_GAP, u64::MAX));
        assert_eq!(merged, vec![0..110]);
        assert_eq!(owner, vec![0, 0]);

        let (merged, _) = coalesce(
            &[0..10, 70_000..70_010],
            ScanKind::Point,
            (POINT_GAP, u64::MAX),
        );
        assert_eq!(merged, vec![0..10, 70_000..70_010]);
    }

    #[test]
    fn dense_merges_gaps_up_to_128_kib_and_the_cap() {
        let (merged, _) = coalesce(
            &[0..10, 131_082..131_090],
            ScanKind::Dense,
            (POINT_GAP, u64::MAX),
        );
        assert_eq!(merged, vec![0..131_090]);
        let (merged, _) = coalesce(
            &[0..10, 131_083..131_090],
            ScanKind::Dense,
            (POINT_GAP, u64::MAX),
        );
        assert_eq!(merged, vec![0..10, 131_083..131_090]);
        let (merged, _) = coalesce(
            &[0..10 * MIB, (10 * MIB + 1)..(20 * MIB + 1)],
            ScanKind::Dense,
            (POINT_GAP, u64::MAX),
        );
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn point_merges_up_to_the_configured_gap() {
        let point = (512 * 1024, u64::MAX);
        let (merged, owner) = coalesce(&[0..10, 500_000..500_010], ScanKind::Point, point);
        assert_eq!(merged, vec![0..500_010]);
        assert_eq!(owner, vec![0, 0]);
        let (merged, _) = coalesce(&[0..10, 600_000..600_010], ScanKind::Point, point);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn sparse_merges_only_small_gaps_whatever_the_point_gap() {
        let wide = (MIB, u64::MAX);
        let (merged, _) = coalesce(&[0..10, 5_000..5_010], ScanKind::Sparse, wide);
        assert_eq!(merged.len(), 2);
        let (merged, _) = coalesce(&[0..10, 4_000..4_010], ScanKind::Sparse, wide);
        assert_eq!(merged, vec![0..4_010]);
    }

    #[test]
    fn point_merges_stop_at_the_configured_range() {
        let point = (512 * 1024, 300_000);
        let (merged, owner) = coalesce(
            &[0..10, 200_000..200_010, 400_000..400_010],
            ScanKind::Point,
            point,
        );
        assert_eq!(merged, vec![0..200_010, 400_000..400_010]);
        assert_eq!(owner, vec![0, 0, 1]);
        // One input larger than the cap stays whole.
        let (merged, _) = coalesce(std::slice::from_ref(&(0..MIB)), ScanKind::Point, point);
        assert_eq!(merged, vec![0..MIB]);
    }

    #[test]
    fn scan_merges_up_to_the_gap_and_the_cap() {
        let (merged, _) = coalesce(
            &[0..10, 500_000..500_010],
            ScanKind::Scan,
            (POINT_GAP, u64::MAX),
        );
        assert_eq!(merged, vec![0..500_010]);

        let (merged, _) = coalesce(
            &[0..10 * MIB, (10 * MIB + 1)..(20 * MIB + 1)],
            ScanKind::Scan,
            (POINT_GAP, u64::MAX),
        );
        assert_eq!(merged.len(), 2);

        let (merged, _) = coalesce(
            std::slice::from_ref(&(0..20 * MIB)),
            ScanKind::Scan,
            (POINT_GAP, u64::MAX),
        );
        assert_eq!(merged, vec![0..20 * MIB]);

        let (merged, owner) =
            coalesce(&[0..20 * MIB, 5..10], ScanKind::Scan, (POINT_GAP, u64::MAX));
        assert_eq!(merged, vec![0..20 * MIB]);
        assert_eq!(owner, vec![0, 0]);
    }

    #[test]
    fn repeated_inputs_share_one_range() {
        let (merged, owner) = coalesce(&[5..9, 5..9], ScanKind::Point, (POINT_GAP, u64::MAX));
        assert_eq!(merged, vec![5..9]);
        assert_eq!(owner, vec![0, 0]);
    }

    #[test]
    fn empty_input_gives_empty_outputs() {
        let (merged, owner) = coalesce(&[], ScanKind::Scan, (POINT_GAP, u64::MAX));
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
                let (merged, owner) = coalesce(&ranges, kind, (POINT_GAP, u64::MAX));
                prop_assert_eq!(owner.len(), ranges.len());
                for (r, &m) in ranges.iter().zip(&owner) {
                    let outer = merged.get(m).expect("owner index in range");
                    prop_assert!(outer.start <= r.start && r.end <= outer.end);
                }
                for pair in merged.windows(2) {
                    prop_assert!(pair[0].end <= pair[1].start);
                }
                if kind != ScanKind::Point && disjoint {
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
