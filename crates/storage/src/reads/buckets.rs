//! Dashboard bucket widths and epoch-aligned UTC `date_bin` expressions.

use cetra_column::{DataType, Interval, ScalarValue};
use cetra_query::{Expr, ScalarFn};

use crate::query_error::RunError;
use crate::views::TimeWindow;

const NS_MINUTE: i64 = 60_000_000_000;
const NS_HOUR: i64 = 60 * NS_MINUTE;

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

/// Output column name of a bucket expression.
pub(crate) const BUCKET: &str = "bucket";

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

/// Resolves `bucket` for `window` into nanoseconds, matching `reads::buckets::resolve`.
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

fn align_down(time: i64, width: i64) -> Result<i64, RunError> {
    let remainder = time.checked_rem_euclid(width).ok_or_else(too_many)?;
    time.checked_sub(remainder).ok_or_else(too_many)
}

fn too_many() -> RunError {
    RunError::InvalidRequest(TOO_MANY_BUCKETS.to_owned())
}

/// `date_bin(width, time, epoch)`: the bucket start of each row, aligned to the
/// Unix epoch, UTC.
pub(crate) fn bucket_expr(width: i64, time_col: Expr) -> Expr {
    Expr::Call {
        func: ScalarFn::DateBin,
        args: vec![
            Expr::Literal(ScalarValue::Interval(Interval {
                months: 0,
                days: 0,
                nanos: width,
            })),
            time_col,
            Expr::Literal(ScalarValue::TimestampNs {
                value: 0,
                utc: true,
            }),
        ],
        data_type: DataType::timestamp_utc(),
    }
}

#[cfg(test)]
mod tests {
    use cetra_column::{
        Batch, CancelToken, Column, DataType, Field, FlatColumn, FlatData, Schema, Selection,
    };
    use cetra_query::{EvalContext, eval_expr};

    use super::*;
    use crate::reads::columns::time_at;

    #[test]
    fn bucket_expr_aligns_timestamps_to_utc_epoch() {
        let width = resolve(
            TimeWindow::new(0, BucketWidth::H1.ns()).expect("window"),
            Bucket::Fixed(BucketWidth::M5),
        )
        .expect("5m");
        assert_eq!(width, 5 * 60 * 1_000_000_000);

        let schema = Schema::try_new_arc(vec![Field::new("ts", DataType::timestamp_utc(), false)])
            .expect("schema");
        let ts_vals = vec![
            0,
            width - 1,
            width,
            width + 123_456_789,
            -1, // negative pre-epoch timestamp aligns down to -width
        ];
        let col = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: ts_vals.clone(),
                    utc: true,
                },
                None,
            )
            .expect("col"),
        );
        let batch =
            Batch::try_new(schema, vec![col], ts_vals.len(), Selection::All).expect("batch");
        let expr = bucket_expr(
            width,
            Expr::Column {
                index: 0,
                name: "ts".to_owned(),
                data_type: DataType::timestamp_utc(),
            },
        );
        let eval_ctx = EvalContext::new(0, CancelToken::new());
        let out_col = eval_expr(&expr, &batch, &eval_ctx, None).expect("eval");
        let out_schema =
            Schema::try_new_arc(vec![Field::new(BUCKET, DataType::timestamp_utc(), false)])
                .expect("out schema");
        let out_batch =
            Batch::try_new_shared(out_schema, vec![out_col], ts_vals.len(), Selection::All)
                .expect("out batch");
        let c = out_batch.column(0).expect("col");
        assert_eq!(time_at(&out_batch, c, 0).expect("0"), 0);
        assert_eq!(time_at(&out_batch, c, 1).expect("1"), 0);
        assert_eq!(time_at(&out_batch, c, 2).expect("2"), width);
        assert_eq!(time_at(&out_batch, c, 3).expect("3"), width);
        assert_eq!(time_at(&out_batch, c, 4).expect("4"), -width);
    }
}
