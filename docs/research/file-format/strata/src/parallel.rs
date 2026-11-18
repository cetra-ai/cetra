//! Running independent CPU-heavy steps of one read on several threads.

use std::ops::Range;

use crate::error::{Result, StrataError};

/// Applies `f` to every item and returns the results in input order, or the first error in
/// input order. The items are cut into contiguous groups of about equal total `weight`, one
/// group per available core, and each group runs on a scoped thread. With a single group, or a
/// single core, everything runs on the calling thread.
pub(crate) fn map_in_order<I: Sync, T: Send>(
    items: &[I],
    weight: impl Fn(&I) -> usize,
    f: impl Fn(&I) -> Result<T> + Sync,
) -> Result<Vec<T>> {
    let run = |group: &[I]| -> Result<Vec<T>> { group.iter().map(&f).collect() };
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let groups = groups(items, weight, threads);
    if groups.len() < 2 {
        return run(items);
    }
    let results: Vec<Result<Vec<T>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = groups
            .iter()
            .map(|g| {
                let group = items.get(g.clone()).unwrap_or_default();
                scope.spawn(move || run(group))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| {
                    Err(StrataError::Schema {
                        reason: "internal: a worker thread panicked".to_string(),
                    })
                })
            })
            .collect()
    });
    let mut out = Vec::with_capacity(items.len());
    for group in results {
        out.extend(group?);
    }
    Ok(out)
}

/// Cuts `items` into at most `n` contiguous ranges, each closed once it reaches its share of
/// the total weight.
fn groups<I>(items: &[I], weight: impl Fn(&I) -> usize, n: usize) -> Vec<Range<usize>> {
    let total: usize = items.iter().map(&weight).sum();
    let share = total.div_ceil(n.max(1)).max(1);
    let mut out = Vec::with_capacity(n);
    let (mut start, mut acc) = (0usize, 0usize);
    for (i, item) in items.iter().enumerate() {
        acc += weight(item);
        // The last group takes whatever is left.
        if acc >= share && out.len() + 1 < n {
            out.push(start..i + 1);
            (start, acc) = (i + 1, 0);
        }
    }
    if start < items.len() {
        out.push(start..items.len());
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn results_keep_the_input_order_and_the_first_error() {
        let items: Vec<u64> = (0..1_000).collect();
        let got = map_in_order(&items, |_| 1, |&k| Ok(k * 2)).unwrap();
        assert_eq!(got, items.iter().map(|k| k * 2).collect::<Vec<_>>());

        let failed = map_in_order(
            &items,
            |_| 1,
            |&k| {
                if k >= 600 {
                    Err(StrataError::Corrupt {
                        what: "test",
                        offset: k,
                    })
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(
            failed,
            Err(StrataError::Corrupt { offset: 600, .. })
        ));
    }

    #[test]
    fn groups_cover_the_items_by_weight() {
        let items = [5usize, 1, 1, 1, 1, 1, 5, 0];
        assert_eq!(groups(&items, |&w| w, 3), vec![0..1, 1..6, 6..8]);
        assert_eq!(groups(&items, |&w| w, 2), vec![0..4, 4..8]);
        assert_eq!(groups(&items, |&w| w, 1), vec![0..8]);
        assert!(groups(&[] as &[usize], |&w| w, 4).is_empty());
    }
}
