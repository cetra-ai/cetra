//! Dashboard bucket widths and the grid of buckets a window gives.

use datafusion::common::ScalarValue;
use datafusion::functions::datetime::expr_fn::date_bin;
use datafusion::logical_expr::{Expr, col};

use crate::query_error::RunError;
use crate::views::TimeWindow;

/// Output column of a bucket expression.
pub(super) const BUCKET: &str = "bucket";

const NS_MINUTE: i64 = 60_000_000_000;
const NS_HOUR: i64 = 60 * NS_MINUTE;
const AUTO_LIMIT: i64 = 120;
const FIXED_LIMIT: i64 = 2_000;
const TOO_MANY_BUCKETS: &str = "too many buckets; pick a wider bucket";

const AUTO_WIDTHS: [BucketWidth; 8] = [
    BucketWidth::M1,
    BucketWidth::M5,
    BucketWidth::M10,
    BucketWidth::M15,
    BucketWidth::M30,
    BucketWidth::H1,
    BucketWidth::H6,
    BucketWidth::H12,
];

/// Width of one dashboard bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketWidth {
    /// One minute.
    M1,
    /// Five minutes.
    M5,
    /// Ten minutes.
    M10,
    /// Fifteen minutes.
    M15,
    /// Thirty minutes.
    M30,
    /// One hour.
    H1,
    /// Six hours.
    H6,
    /// Twelve hours.
    H12,
    /// One day, UTC.
    D1,
}

impl BucketWidth {
    /// Width in nanoseconds.
    pub fn ns(self) -> i64 {
        match self {
            Self::M1 => NS_MINUTE,
            Self::M5 => 5 * NS_MINUTE,
            Self::M10 => 10 * NS_MINUTE,
            Self::M15 => 15 * NS_MINUTE,
            Self::M30 => 30 * NS_MINUTE,
            Self::H1 => NS_HOUR,
            Self::H6 => 6 * NS_HOUR,
            Self::H12 => 12 * NS_HOUR,
            Self::D1 => 24 * NS_HOUR,
        }
    }
}

/// How the dashboard picks a bucket width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    /// Smallest width of at most 120 buckets, else one day.
    Auto,
    /// The given width.
    Fixed(BucketWidth),
}

pub(crate) fn resolve(window: TimeWindow, bucket: Bucket) -> Result<i64, RunError> {
    match bucket {
        Bucket::Fixed(width) => {
            let count = bucket_count(window, width.ns())?;
            if !(1..=FIXED_LIMIT).contains(&count) {
                return Err(too_many());
            }
            Ok(width.ns())
        }
        Bucket::Auto => {
            for width in AUTO_WIDTHS {
                if bucket_count(window, width.ns())? <= AUTO_LIMIT {
                    return Ok(width.ns());
                }
            }
            let width = BucketWidth::D1.ns();
            if bucket_count(window, width)? > FIXED_LIMIT {
                return Err(too_many());
            }
            Ok(width)
        }
    }
}

fn bucket_count(window: TimeWindow, width: i64) -> Result<i64, RunError> {
    let first = align_down(window.from(), width)?;
    let last_tick = window.to().checked_sub(1).ok_or_else(too_many)?;
    let last = align_down(last_tick, width)?;
    let span = last.checked_sub(first).ok_or_else(too_many)?;
    if span.checked_rem_euclid(width) != Some(0) {
        return Err(too_many());
    }
    span.checked_div(width)
        .and_then(|steps| steps.checked_add(1))
        .ok_or_else(too_many)
}

/// `date_bin(width, time, epoch)` as [`BUCKET`]: the bucket start of each row, aligned to the
/// Unix epoch, UTC.
pub(super) fn bucket_expr(width: i64, time: &str) -> Expr {
    date_bin(
        Expr::Literal(ScalarValue::new_interval_mdn(0, 0, width), None),
        col(time),
        Expr::Literal(
            ScalarValue::TimestampNanosecond(Some(0), Some("UTC".into())),
            None,
        ),
    )
    .alias(BUCKET)
}

fn align_down(time: i64, width: i64) -> Result<i64, RunError> {
    let remainder = time.checked_rem_euclid(width).ok_or_else(too_many)?;
    time.checked_sub(remainder).ok_or_else(too_many)
}

fn too_many() -> RunError {
    RunError::InvalidRequest(TOO_MANY_BUCKETS.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{Bucket, BucketWidth, resolve};
    use crate::query_error::RunError;
    use crate::views::TimeWindow;

    #[test]
    fn auto_buckets_stop_at_two_thousand_days() {
        let day = BucketWidth::D1.ns();
        let window = |from, to| TimeWindow::new(from, to).expect("window");
        assert_eq!(
            resolve(window(0, 2_000 * day), Bucket::Auto).expect("2,000 days"),
            day
        );
        for to in [2_000 * day + 1, i64::MAX] {
            match resolve(window(0, to), Bucket::Auto) {
                Err(RunError::InvalidRequest(message)) => {
                    assert_eq!(message, "too many buckets; pick a wider bucket");
                }
                other => panic!("{other:?}"),
            }
        }
        assert!(matches!(
            resolve(window(i64::MIN, i64::MAX), Bucket::Auto),
            Err(RunError::InvalidRequest(_))
        ));
    }

    #[test]
    fn exactly_two_thousand_fixed_buckets_are_accepted() {
        let span = 2_000i64.checked_mul(BucketWidth::M1.ns()).expect("span");
        assert_eq!(
            resolve(
                TimeWindow::new(0, span).expect("window"),
                Bucket::Fixed(BucketWidth::M1)
            )
            .expect("2,000 buckets"),
            BucketWidth::M1.ns()
        );
    }
}
