//! Clustering math for issue clustering (D-IC-7, D-IC-8, D-IC-9): unit-vector cosine,
//! nearest centroid, running centroids, and deterministic average-linkage clustering.
//!
//! Pure functions. The server calls them from `spawn_blocking`.

/// Largest input to [`average_linkage`] (D-IC-8).
pub const MAX_CLUSTER_INPUT: usize = 5_000;

/// Absorbs f32 rounding when the merge distance of identical vectors is compared with the cut.
const CUT_EPSILON: f32 = 1e-6;

/// Why a clustering call was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ClusterError {
    /// Two vectors in one call have different lengths.
    #[error("vectors have different dimensions")]
    DimensionMismatch,
    /// [`average_linkage`] was given more than [`MAX_CLUSTER_INPUT`] vectors.
    #[error("too many vectors to cluster")]
    TooMany,
    /// An input holds a NaN or infinite component. Such distances never form a reciprocal pair.
    #[error("vectors must be finite")]
    NonFinite,
}

/// True when every component of `v` is finite.
fn is_finite(v: &[f32]) -> bool {
    v.iter().all(|value| value.is_finite())
}

/// Scales `v` to unit length. Returns false, leaving `v` unchanged, when its norm is 0 or not finite.
pub fn l2_normalize(v: &mut [f32]) -> bool {
    let norm = v
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    if !(norm.is_finite() && norm > 0.0) {
        return false;
    }
    for value in v.iter_mut() {
        *value = (f64::from(*value) / norm) as f32;
    }
    true
}

/// Dot product of two unit vectors, which is their cosine similarity.
///
/// # Errors
///
/// Returns [`ClusterError::DimensionMismatch`] when the lengths differ, and
/// [`ClusterError::NonFinite`] when either vector holds a NaN or infinite value.
pub fn cosine(a: &[f32], b: &[f32]) -> Result<f32, ClusterError> {
    if a.len() != b.len() {
        return Err(ClusterError::DimensionMismatch);
    }
    if !is_finite(a) || !is_finite(b) {
        return Err(ClusterError::NonFinite);
    }
    let dot: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum();
    Ok((dot as f32).clamp(-1.0, 1.0))
}

/// Index and similarity of the centroid nearest to `v`. Ties go to the lowest index. `None` when empty.
///
/// # Errors
///
/// Returns [`ClusterError::DimensionMismatch`] when any centroid has a different length from `v`,
/// and [`ClusterError::NonFinite`] when `v` or a centroid holds a NaN or infinite value.
pub fn nearest<'a, I>(v: &[f32], centroids: I) -> Result<Option<(usize, f32)>, ClusterError>
where
    I: IntoIterator<Item = &'a [f32]>,
{
    if !is_finite(v) {
        return Err(ClusterError::NonFinite);
    }
    let mut best: Option<(usize, f32)> = None;
    for (index, centroid) in centroids.into_iter().enumerate() {
        let similarity = cosine(v, centroid)?;
        if best.is_none_or(|(_, current)| similarity > current) {
            best = Some((index, similarity));
        }
    }
    Ok(best)
}

/// Moves `centroid` (the mean of `members` unit vectors, renormalized) towards `v`:
/// `c += (v − c) / (members + 1)`, then renormalized.
///
/// When the result cannot be normalized, `centroid` becomes a copy of `v`.
///
/// # Errors
///
/// Returns [`ClusterError::DimensionMismatch`] when the lengths differ, and
/// [`ClusterError::NonFinite`] when `centroid` or `v` holds a NaN or infinite value.
pub fn add_to_centroid(centroid: &mut [f32], members: u64, v: &[f32]) -> Result<(), ClusterError> {
    if centroid.len() != v.len() {
        return Err(ClusterError::DimensionMismatch);
    }
    if !is_finite(centroid) || !is_finite(v) {
        return Err(ClusterError::NonFinite);
    }
    let k = 1.0 / (members as f64 + 1.0);
    for (c, x) in centroid.iter_mut().zip(v) {
        *c = (f64::from(*c) + (f64::from(*x) - f64::from(*c)) * k) as f32;
    }
    if !l2_normalize(centroid) {
        centroid.copy_from_slice(v);
    }
    Ok(())
}

/// The member-weighted mean of two centroids, renormalized (merges, D-IC-9).
///
/// With no members on either side the weights are equal. When the weighted sum cannot be
/// normalized (for example opposite vectors cancel to zero), returns a copy of the heavier
/// centroid, `a` on a tie.
///
/// # Errors
///
/// Returns [`ClusterError::DimensionMismatch`] when the lengths differ, and
/// [`ClusterError::NonFinite`] when `a` or `b` holds a NaN or infinite value.
pub fn merge_centroids(
    a: &[f32],
    a_members: u64,
    b: &[f32],
    b_members: u64,
) -> Result<Vec<f32>, ClusterError> {
    if a.len() != b.len() {
        return Err(ClusterError::DimensionMismatch);
    }
    if !is_finite(a) || !is_finite(b) {
        return Err(ClusterError::NonFinite);
    }
    let (weight_a, weight_b) = if a_members == 0 && b_members == 0 {
        (1.0_f64, 1.0_f64)
    } else {
        (a_members as f64, b_members as f64)
    };
    let total = weight_a + weight_b;
    let (weight_a, weight_b) = (weight_a / total, weight_b / total);
    let mut merged: Vec<f32> = a
        .iter()
        .zip(b)
        .map(|(x, y)| (f64::from(*x) * weight_a + f64::from(*y) * weight_b) as f32)
        .collect();
    if l2_normalize(&mut merged) {
        return Ok(merged);
    }
    Ok(if a_members >= b_members {
        a.to_vec()
    } else {
        b.to_vec()
    })
}

/// Average-linkage clusters of unit vectors whose merge distance (1 − cosine) is at most
/// `max_distance`. Each cluster lists input indices in ascending order. Clusters are ordered by
/// their first index. Every input index is in exactly one cluster (singletons included).
///
/// The condensed distance matrix takes O(n²) memory, at most about 50 MB at [`MAX_CLUSTER_INPUT`].
/// The function is synchronous and is run from `spawn_blocking`.
///
/// # Errors
///
/// Returns [`ClusterError::TooMany`] above [`MAX_CLUSTER_INPUT`] vectors,
/// [`ClusterError::DimensionMismatch`] when the vectors do not all have the first one's length,
/// and [`ClusterError::NonFinite`] when any component of any vector is NaN or infinite.
pub fn average_linkage(
    vectors: &[Vec<f32>],
    max_distance: f32,
) -> Result<Vec<Vec<usize>>, ClusterError> {
    let n = vectors.len();
    if n > MAX_CLUSTER_INPUT {
        return Err(ClusterError::TooMany);
    }
    let dim = vectors.first().map_or(0, Vec::len);
    if vectors.iter().any(|vector| vector.len() != dim) {
        return Err(ClusterError::DimensionMismatch);
    }
    if vectors.iter().any(|vector| !is_finite(vector)) {
        return Err(ClusterError::NonFinite);
    }
    if n == 0 {
        return Ok(Vec::new());
    }
    if n == 1 {
        return Ok(vec![vec![0]]);
    }

    let mut dist = Vec::with_capacity(n * (n - 1) / 2);
    for (i, a) in vectors.iter().enumerate() {
        for b in &vectors[i + 1..] {
            dist.push(1.0 - cosine(a, b)?);
        }
    }

    let merges = nn_chain(dist, n);

    let cut = max_distance + CUT_EPSILON;
    let mut parent: Vec<usize> = (0..n).collect();
    for (a, b, distance) in merges {
        if distance <= cut {
            let root_a = find(&mut parent, a);
            let root_b = find(&mut parent, b);
            if root_a != root_b {
                parent[root_b.max(root_a)] = root_a.min(root_b);
            }
        }
    }

    let mut group_of_root: Vec<Option<usize>> = vec![None; n];
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        match group_of_root[root] {
            Some(group) => groups[group].push(i),
            None => {
                group_of_root[root] = Some(groups.len());
                groups.push(vec![i]);
            }
        }
    }
    Ok(groups)
}

/// Index of the pair `{i, j}` in the condensed matrix of `n` items. `i` and `j` must differ.
fn idx(n: usize, i: usize, j: usize) -> usize {
    let (i, j) = if i < j { (i, j) } else { (j, i) };
    n * i - i * (i + 1) / 2 + (j - i - 1)
}

/// Average-linkage merges `(a, b, distance)` of the condensed matrix `dist`, found by the
/// nearest-neighbour chain. `dist` is consumed and updated with Lance–Williams averages.
fn nn_chain(mut dist: Vec<f32>, n: usize) -> Vec<(usize, usize, f32)> {
    let mut size = vec![1_u64; n];
    let mut active = vec![true; n];
    let mut active_count = n;
    let mut chain: Vec<usize> = Vec::new();
    let mut merges: Vec<(usize, usize, f32)> = Vec::with_capacity(n - 1);

    while active_count > 1 {
        if chain.is_empty() {
            // Invariant: more than one cluster is active, so an active index exists.
            let Some(start) = active.iter().position(|&live| live) else {
                break;
            };
            chain.push(start);
        }
        let Some(&a) = chain.last() else {
            break;
        };
        let previous = if chain.len() >= 2 {
            chain.get(chain.len() - 2).copied()
        } else {
            None
        };

        // Nearest active neighbour of `a`. Ties prefer `previous`, else the lowest index.
        let mut best: Option<(usize, f32)> = None;
        for k in (0..n).filter(|&k| k != a && active[k]) {
            let d = dist[idx(n, a, k)];
            let better = match best {
                None => true,
                Some((_, best_d)) => d < best_d || (d == best_d && Some(k) == previous),
            };
            if better {
                best = Some((k, d));
            }
        }
        let Some((b, distance)) = best else {
            break;
        };

        if Some(b) == previous {
            chain.pop();
            chain.pop();
            let keep = a.min(b);
            let gone = a.max(b);
            let size_a = size[a];
            let size_b = size[b];
            let total = (size_a + size_b) as f64;
            for k in (0..n).filter(|&k| k != a && k != b && active[k]) {
                let d_ka = f64::from(dist[idx(n, k, a)]);
                let d_kb = f64::from(dist[idx(n, k, b)]);
                let updated = (size_a as f64 * d_ka + size_b as f64 * d_kb) / total;
                dist[idx(n, k, keep)] = updated as f32;
            }
            merges.push((a, b, distance));
            size[keep] = size_a + size_b;
            active[gone] = false;
            active_count -= 1;
        } else {
            chain.push(b);
        }
    }
    merges
}

/// Root of `x`, with path halving.
fn find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn unit(values: &[f32]) -> Vec<f32> {
        let mut v = values.to_vec();
        assert!(l2_normalize(&mut v));
        v
    }

    fn assert_close(actual: &[f32], expected: &[f32]) {
        assert_eq!(actual.len(), expected.len());
        for (a, e) in actual.iter().zip(expected) {
            assert!((a - e).abs() < 1e-6, "{actual:?} != {expected:?}");
        }
    }

    /// A unit vector, or the first axis when the input is zero.
    fn to_unit(mut v: Vec<f32>) -> Vec<f32> {
        if !l2_normalize(&mut v) {
            v.fill(0.0);
            v[0] = 1.0;
        }
        v
    }

    /// Up to 40 unit vectors of one dimension from 2 to 8, with a cut in [0, 1].
    fn unit_set() -> impl Strategy<Value = (Vec<Vec<f32>>, f32)> {
        (2_usize..=8)
            .prop_flat_map(|dim| {
                (
                    prop::collection::vec(prop::collection::vec(-1.0_f32..1.0, dim), 0..=40),
                    0.0_f32..=1.0,
                )
            })
            .prop_map(|(vectors, cut)| (vectors.into_iter().map(to_unit).collect(), cut))
    }

    /// Test-only O(n³) average linkage. Returns the partition and the smallest distance from
    /// the cut over every height it examined, so the caller can skip near-ties.
    fn naive_average_linkage(vectors: &[Vec<f32>], cut: f32) -> (Vec<Vec<usize>>, f32) {
        let dist = |i: usize, j: usize| 1.0 - cosine(&vectors[i], &vectors[j]).unwrap();
        let mut clusters: Vec<Vec<usize>> = (0..vectors.len()).map(|i| vec![i]).collect();
        let mut closest_to_cut = f32::INFINITY;
        while clusters.len() >= 2 {
            let mut best: Option<(usize, usize, f64)> = None;
            for p in 0..clusters.len() {
                for q in p + 1..clusters.len() {
                    let mut sum = 0.0_f64;
                    let mut count = 0_usize;
                    for &i in &clusters[p] {
                        for &j in &clusters[q] {
                            sum += f64::from(dist(i, j));
                            count += 1;
                        }
                    }
                    let mean = sum / count as f64;
                    if best.is_none_or(|(_, _, current)| mean < current) {
                        best = Some((p, q, mean));
                    }
                }
            }
            let Some((p, q, height)) = best else {
                break;
            };
            closest_to_cut = closest_to_cut.min((height - f64::from(cut)).abs() as f32);
            if height > f64::from(cut) + f64::from(CUT_EPSILON) {
                break;
            }
            let moved = clusters.remove(q);
            clusters[p].extend(moved);
        }
        for cluster in &mut clusters {
            cluster.sort_unstable();
        }
        clusters.sort_by_key(|cluster| cluster[0]);
        (clusters, closest_to_cut)
    }

    #[test]
    fn l2_normalize_rejects_zero_and_non_finite() {
        let mut zero = vec![0.0_f32, 0.0, 0.0];
        assert!(!l2_normalize(&mut zero));
        assert_eq!(zero, vec![0.0, 0.0, 0.0]);

        let mut empty: Vec<f32> = Vec::new();
        assert!(!l2_normalize(&mut empty));

        let mut nan = vec![f32::NAN, 1.0];
        assert!(!l2_normalize(&mut nan));
        assert!(nan[1] == 1.0 && nan[0].is_nan());

        let mut infinite = vec![f32::INFINITY, 1.0];
        assert!(!l2_normalize(&mut infinite));
        assert_eq!(infinite, vec![f32::INFINITY, 1.0]);

        let mut ok = vec![3.0_f32, 4.0];
        assert!(l2_normalize(&mut ok));
        assert_close(&ok, &[0.6, 0.8]);
    }

    #[test]
    fn cosine_of_identical_unit_vectors_is_one() {
        let axis = [1.0_f32, 0.0, 0.0];
        assert_eq!(cosine(&axis, &axis), Ok(1.0));

        let v = unit(&[0.3, -1.2, 0.7]);
        assert!((cosine(&v, &v).unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_and_nearest_reject_dimension_mismatch() {
        assert_eq!(
            cosine(&[1.0, 0.0], &[1.0, 0.0, 0.0]),
            Err(ClusterError::DimensionMismatch)
        );
        let v = [1.0_f32, 0.0];
        let short = [1.0_f32, 0.0];
        let long = [1.0_f32, 0.0, 0.0];
        let centroids: Vec<&[f32]> = vec![&short, &long];
        assert_eq!(nearest(&v, centroids), Err(ClusterError::DimensionMismatch));
    }

    #[test]
    fn cosine_rejects_non_finite() {
        let finite = [1.0_f32, 0.0];
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let poisoned = [bad, 0.0];
            assert_eq!(cosine(&poisoned, &finite), Err(ClusterError::NonFinite));
            assert_eq!(cosine(&finite, &poisoned), Err(ClusterError::NonFinite));
        }
    }

    #[test]
    fn centroid_functions_reject_non_finite() {
        let finite = [1.0_f32, 0.0];
        let poisoned = [f32::NAN, 1.0];
        let centroids: Vec<&[f32]> = vec![&poisoned];
        assert_eq!(nearest(&finite, centroids), Err(ClusterError::NonFinite));
        assert_eq!(
            nearest(&poisoned, Vec::<&[f32]>::new()),
            Err(ClusterError::NonFinite)
        );

        let mut centroid = vec![1.0_f32, 0.0];
        assert_eq!(
            add_to_centroid(&mut centroid, 1, &poisoned),
            Err(ClusterError::NonFinite)
        );
        assert_eq!(centroid, vec![1.0, 0.0]);

        assert_eq!(
            merge_centroids(&finite, 1, &poisoned, 1),
            Err(ClusterError::NonFinite)
        );
    }

    #[test]
    fn average_linkage_rejects_nan_and_inf() {
        let finite = unit(&[1.0, 0.0]);
        let other = unit(&[0.0, 1.0]);
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            // Anywhere in the input, including the last vector and the one-vector case.
            let poisoned_last = vec![finite.clone(), other.clone(), vec![bad, 0.0]];
            assert_eq!(
                average_linkage(&poisoned_last, 0.5),
                Err(ClusterError::NonFinite)
            );
            let poisoned_only = vec![vec![bad, 0.0]];
            assert_eq!(
                average_linkage(&poisoned_only, 0.5),
                Err(ClusterError::NonFinite)
            );
        }
    }

    #[test]
    fn nearest_breaks_ties_on_the_lowest_index() {
        let v = [1.0_f32, 0.0];
        let first = [1.0_f32, 0.0];
        let second = [1.0_f32, 0.0];
        let other = [0.0_f32, 1.0];
        let centroids: Vec<&[f32]> = vec![&other, &first, &second];
        assert_eq!(nearest(&v, centroids), Ok(Some((1, 1.0))));
    }

    #[test]
    fn nearest_of_no_centroids_is_none() {
        let centroids: Vec<&[f32]> = Vec::new();
        assert_eq!(nearest(&[1.0, 0.0], centroids), Ok(None));
    }

    #[test]
    fn add_to_centroid_is_the_running_mean() {
        let mut centroid = vec![1.0_f32, 0.0];
        add_to_centroid(&mut centroid, 1, &[0.0, 1.0]).unwrap();
        assert_close(&centroid, &unit(&[0.5, 0.5]));
    }

    #[test]
    fn add_to_centroid_of_opposite_vectors_takes_the_new_vector() {
        let mut centroid = vec![1.0_f32, 0.0];
        add_to_centroid(&mut centroid, 1, &[-1.0, 0.0]).unwrap();
        assert_eq!(centroid, vec![-1.0, 0.0]);
    }

    #[test]
    fn add_to_centroid_rejects_dimension_mismatch() {
        let mut centroid = vec![1.0_f32, 0.0];
        assert_eq!(
            add_to_centroid(&mut centroid, 1, &[0.0, 1.0, 0.0]),
            Err(ClusterError::DimensionMismatch)
        );
    }

    #[test]
    fn merge_centroids_weights_by_members() {
        let merged = merge_centroids(&[1.0, 0.0], 3, &[0.0, 1.0], 1).unwrap();
        assert_close(&merged, &unit(&[3.0, 1.0]));
    }

    #[test]
    fn merge_centroids_of_no_members_uses_equal_weights() {
        let merged = merge_centroids(&[1.0, 0.0], 0, &[0.0, 1.0], 0).unwrap();
        assert_close(&merged, &unit(&[1.0, 1.0]));
    }

    #[test]
    fn merge_centroids_of_opposite_vectors_keeps_the_heavier() {
        let heavier_b = merge_centroids(&[1.0, 0.0], 1, &[-1.0, 0.0], 2).unwrap();
        assert_eq!(heavier_b, vec![-1.0, 0.0]);

        let tie = merge_centroids(&[1.0, 0.0], 2, &[-1.0, 0.0], 2).unwrap();
        assert_eq!(tie, vec![1.0, 0.0]);
    }

    #[test]
    fn merge_centroids_rejects_dimension_mismatch() {
        assert_eq!(
            merge_centroids(&[1.0, 0.0], 1, &[1.0], 1),
            Err(ClusterError::DimensionMismatch)
        );
    }

    #[test]
    fn average_linkage_two_tight_groups() {
        let e1a = unit(&[1.0, 0.05, 0.0]);
        let e1b = unit(&[1.0, 0.0, 0.05]);
        let e1c = unit(&[1.0, 0.05, 0.05]);
        let e2a = unit(&[0.05, 1.0, 0.0]);
        let e2b = unit(&[0.0, 1.0, 0.05]);
        let e2c = unit(&[0.05, 1.0, 0.05]);
        // Interleaved, so the output order is checked against the input order.
        let vectors = vec![e1a, e2a, e1b, e2b, e1c, e2c];
        assert_eq!(
            average_linkage(&vectors, 0.2),
            Ok(vec![vec![0, 2, 4], vec![1, 3, 5]])
        );
    }

    #[test]
    fn average_linkage_cut_zero_keeps_singletons_unless_identical() {
        let vectors = vec![
            unit(&[1.0, 0.0, 0.0]),
            unit(&[1.0, 0.0, 0.0]),
            unit(&[0.9, 0.1, 0.0]),
        ];
        assert_eq!(
            average_linkage(&vectors, 0.0),
            Ok(vec![vec![0, 1], vec![2]])
        );
    }

    #[test]
    fn average_linkage_empty_and_single() {
        assert_eq!(average_linkage(&[], 0.5), Ok(Vec::new()));
        assert_eq!(
            average_linkage(&[unit(&[1.0, 0.0])], 0.5),
            Ok(vec![vec![0]])
        );
    }

    #[test]
    fn average_linkage_too_many() {
        let vectors = vec![vec![1.0_f32]; MAX_CLUSTER_INPUT + 1];
        assert_eq!(average_linkage(&vectors, 0.5), Err(ClusterError::TooMany));
    }

    #[test]
    fn average_linkage_rejects_dimension_mismatch() {
        let vectors = vec![vec![1.0_f32, 0.0], vec![1.0_f32]];
        assert_eq!(
            average_linkage(&vectors, 0.5),
            Err(ClusterError::DimensionMismatch)
        );
    }

    #[test]
    fn average_linkage_is_deterministic() {
        let vectors = vec![
            unit(&[1.0, 0.0]),
            unit(&[1.0, 0.0]),
            unit(&[0.0, 1.0]),
            unit(&[0.7, 0.7]),
            unit(&[-1.0, 0.2]),
            unit(&[0.0, 1.0]),
        ];
        let first = average_linkage(&vectors, 0.5);
        assert!(first.is_ok());
        assert_eq!(first, average_linkage(&vectors, 0.5));
    }

    proptest! {
        #![proptest_config(crate::hostile_tests::config())]

        #[test]
        fn average_linkage_matches_a_naive_reference((vectors, cut) in unit_set()) {
            let (reference, closest_to_cut) = naive_average_linkage(&vectors, cut);
            prop_assume!(closest_to_cut > 1e-4);
            prop_assert_eq!(average_linkage(&vectors, cut), Ok(reference));
        }

        #[test]
        fn average_linkage_partition_covers_every_index_once((vectors, cut) in unit_set()) {
            prop_assert!(average_linkage(&vectors, cut).is_ok());
            let clusters = average_linkage(&vectors, cut).unwrap();
            let mut flat: Vec<usize> = clusters.iter().flatten().copied().collect();
            flat.sort_unstable();
            prop_assert_eq!(flat, (0..vectors.len()).collect::<Vec<usize>>());
            for cluster in &clusters {
                prop_assert!(cluster.windows(2).all(|pair| pair[0] < pair[1]));
            }
            prop_assert!(clusters.windows(2).all(|pair| pair[0][0] < pair[1][0]));
        }
    }
}
