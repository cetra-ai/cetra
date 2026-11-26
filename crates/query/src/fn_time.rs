//! Calendar-aware time, interval, truncation, binning, and extraction functions.

use cetra_column::{
    Bitmap, Column, ColumnError, DataType, FlatColumn, FlatData, Interval, Reservation, ScalarValue,
};
use chrono::{
    DateTime, Datelike, FixedOffset, Months, NaiveDate, NaiveTime, TimeZone, Timelike, Utc,
};

use crate::error::QueryError;
use crate::eval::{
    check_cancel, checked_fixed_bytes, is_active, pool_size, read_date, read_f64, read_i64,
    read_interval, read_str, read_timestamp, reconcile_output, rollback_to, try_reserve,
};
use crate::ir::{EvalContext, ScalarFn};

const CANCEL_CHECK_MASK: usize = 1023;
const NS_PER_MICRO: i64 = 1_000;
const NS_PER_MILLI: i64 = 1_000_000;
const NS_PER_SEC: i64 = 1_000_000_000;
const NS_PER_MIN: i64 = 60 * NS_PER_SEC;
const NS_PER_HOUR: i64 = 60 * NS_PER_MIN;
const NS_PER_DAY: i64 = 24 * NS_PER_HOUR;

const UNIX_EPOCH_CE_DAYS: i32 = 719_163;
const DAYS_EPOCH_SHIFT: i64 = 719_468;
const DAYS_PER_ERA: i64 = 146_097;

const INTERVAL_PRECISION: u32 = 15;
const INTERVAL_SCALE: i128 = 1_000_000_000_000_000;

// ---------------------------------------------------------------------------
// Interval parsing & calendar arithmetic
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
enum IntervalUnit {
    Century = 1 << 0,
    Decade = 1 << 1,
    Year = 1 << 2,
    Month = 1 << 3,
    Week = 1 << 4,
    Day = 1 << 5,
    Hour = 1 << 6,
    Minute = 1 << 7,
    Second = 1 << 8,
    Millisecond = 1 << 9,
    Microsecond = 1 << 10,
    Nanosecond = 1 << 11,
}

impl IntervalUnit {
    fn parse(s: &str) -> Result<Self, QueryError> {
        match s.to_ascii_lowercase().as_str() {
            "c" | "cent" | "cents" | "century" | "centuries" => Ok(Self::Century),
            "dec" | "decs" | "decade" | "decades" => Ok(Self::Decade),
            "y" | "yr" | "yrs" | "year" | "years" => Ok(Self::Year),
            "mon" | "mons" | "month" | "months" => Ok(Self::Month),
            "w" | "week" | "weeks" => Ok(Self::Week),
            "d" | "day" | "days" => Ok(Self::Day),
            "h" | "hr" | "hrs" | "hour" | "hours" => Ok(Self::Hour),
            "m" | "min" | "mins" | "minute" | "minutes" => Ok(Self::Minute),
            "s" | "sec" | "secs" | "second" | "seconds" => Ok(Self::Second),
            "ms" | "msec" | "msecs" | "msecond" | "mseconds" | "millisecond" | "milliseconds" => {
                Ok(Self::Millisecond)
            }
            "us" | "usec" | "usecs" | "usecond" | "useconds" | "microsecond" | "microseconds" => {
                Ok(Self::Microsecond)
            }
            "ns" | "nsec" | "nsecs" | "nsecond" | "nseconds" | "nanosecond" | "nanoseconds" => {
                Ok(Self::Nanosecond)
            }
            _ => Err(QueryError::failed("unknown interval unit")),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct IntervalAmount {
    integer: i64,
    frac: i64,
}

fn parse_interval_amount(s: &str) -> Result<IntervalAmount, QueryError> {
    if s.is_empty() {
        return Err(QueryError::failed("invalid interval amount"));
    }
    match s.split_once('.') {
        Some((int_part, frac_part)) => {
            if frac_part.is_empty()
                || frac_part.len() > INTERVAL_PRECISION as usize
                || !frac_part.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(QueryError::failed("invalid interval fractional amount"));
            }
            let explicit_neg = int_part.starts_with('-');
            let integer = if int_part.is_empty() || int_part == "-" || int_part == "+" {
                0
            } else {
                int_part
                    .parse::<i64>()
                    .map_err(|_| QueryError::failed("invalid interval amount"))?
            };
            let frac_raw = frac_part
                .parse::<i64>()
                .map_err(|_| QueryError::failed("invalid interval fractional amount"))?;
            let scale = 10i64.pow(INTERVAL_PRECISION - frac_part.len() as u32);
            let frac_mag = frac_raw
                .checked_mul(scale)
                .ok_or_else(|| QueryError::failed("interval amount overflow"))?;
            let frac = if integer < 0 || explicit_neg {
                -frac_mag
            } else {
                frac_mag
            };
            Ok(IntervalAmount { integer, frac })
        }
        None => {
            let integer = s
                .parse::<i64>()
                .map_err(|_| QueryError::failed("invalid interval amount"))?;
            Ok(IntervalAmount { integer, frac: 0 })
        }
    }
}

fn is_interval_amount_char(c: char) -> bool {
    c.is_ascii_digit() || c == '.' || c == '-' || c == '+'
}

/// Parses a PostgreSQL / DataFusion interval string into an [`Interval`].
///
/// Supports compound units (`'2 years 3 days 4 hours 5 seconds'`), negative values,
/// fractional units (`'1.5 days'`), singular/plural/abbreviated names (`ns`, `us`, `ms`,
/// `s`/`sec`, `m`/`min`, `h`/`hr`, `d`, `w`, `mon`, `y`/`yr`), and bare numeric values
/// (defaulting to seconds, matching DataFusion).
pub fn parse_interval(text: &str) -> Result<Interval, QueryError> {
    let trimmed = text.trim_ascii();
    if trimmed.is_empty() {
        return Err(QueryError::failed("empty interval string"));
    }

    let mut pairs: Vec<(IntervalAmount, IntervalUnit)> = Vec::new();
    let mut words = trimmed.split_whitespace();
    while let Some(word) = words.next() {
        let (amt_str, unit) = if let Some(split_at) = word.find(|c| !is_interval_amount_char(c)) {
            let (a, u) = word.split_at(split_at);
            (a, IntervalUnit::parse(u)?)
        } else if let Some(next_word) = words.next() {
            (word, IntervalUnit::parse(next_word)?)
        } else {
            (word, IntervalUnit::Second)
        };
        let amount = parse_interval_amount(amt_str)?;
        pairs.push((amount, unit));
    }

    if pairs.is_empty() {
        return Err(QueryError::failed("empty interval string"));
    }

    let mut seen_units: u16 = 0;
    let mut months: i32 = 0;
    let mut days: i32 = 0;
    let mut nanos: i64 = 0;

    for (amount, unit) in pairs {
        let bit = unit as u16;
        if (seen_units & bit) != 0 {
            return Err(QueryError::failed("repeated unit in interval string"));
        }
        seen_units |= bit;

        let int128 = i128::from(amount.integer);
        let frac128 = i128::from(amount.frac);

        match unit {
            IntervalUnit::Century => {
                let m = int128 * 1200 + (frac128 * 1200) / INTERVAL_SCALE;
                let m32 =
                    i32::try_from(m).map_err(|_| QueryError::failed("interval months overflow"))?;
                months = months
                    .checked_add(m32)
                    .ok_or_else(|| QueryError::failed("interval months overflow"))?;
            }
            IntervalUnit::Decade => {
                let m = int128 * 120 + (frac128 * 120) / INTERVAL_SCALE;
                let m32 =
                    i32::try_from(m).map_err(|_| QueryError::failed("interval months overflow"))?;
                months = months
                    .checked_add(m32)
                    .ok_or_else(|| QueryError::failed("interval months overflow"))?;
            }
            IntervalUnit::Year => {
                let m = int128 * 12 + (frac128 * 12) / INTERVAL_SCALE;
                let m32 =
                    i32::try_from(m).map_err(|_| QueryError::failed("interval months overflow"))?;
                months = months
                    .checked_add(m32)
                    .ok_or_else(|| QueryError::failed("interval months overflow"))?;
            }
            IntervalUnit::Month => {
                let m32 = i32::try_from(amount.integer)
                    .map_err(|_| QueryError::failed("interval months overflow"))?;
                let d = (frac128 * 30) / INTERVAL_SCALE;
                let d32 =
                    i32::try_from(d).map_err(|_| QueryError::failed("interval days overflow"))?;
                months = months
                    .checked_add(m32)
                    .ok_or_else(|| QueryError::failed("interval months overflow"))?;
                days = days
                    .checked_add(d32)
                    .ok_or_else(|| QueryError::failed("interval days overflow"))?;
            }
            IntervalUnit::Week => {
                let d = int128 * 7;
                let d32 =
                    i32::try_from(d).map_err(|_| QueryError::failed("interval days overflow"))?;
                let n = (frac128 * 7 * i128::from(NS_PER_DAY)) / INTERVAL_SCALE;
                let n64 =
                    i64::try_from(n).map_err(|_| QueryError::failed("interval nanos overflow"))?;
                days = days
                    .checked_add(d32)
                    .ok_or_else(|| QueryError::failed("interval days overflow"))?;
                nanos = nanos
                    .checked_add(n64)
                    .ok_or_else(|| QueryError::failed("interval nanos overflow"))?;
            }
            IntervalUnit::Day => {
                let d32 = i32::try_from(amount.integer)
                    .map_err(|_| QueryError::failed("interval days overflow"))?;
                let n = (frac128 * i128::from(NS_PER_DAY)) / INTERVAL_SCALE;
                let n64 =
                    i64::try_from(n).map_err(|_| QueryError::failed("interval nanos overflow"))?;
                days = days
                    .checked_add(d32)
                    .ok_or_else(|| QueryError::failed("interval days overflow"))?;
                nanos = nanos
                    .checked_add(n64)
                    .ok_or_else(|| QueryError::failed("interval nanos overflow"))?;
            }
            IntervalUnit::Hour => {
                let n = int128 * i128::from(NS_PER_HOUR)
                    + (frac128 * i128::from(NS_PER_HOUR)) / INTERVAL_SCALE;
                let n64 =
                    i64::try_from(n).map_err(|_| QueryError::failed("interval nanos overflow"))?;
                nanos = nanos
                    .checked_add(n64)
                    .ok_or_else(|| QueryError::failed("interval nanos overflow"))?;
            }
            IntervalUnit::Minute => {
                let n = int128 * i128::from(NS_PER_MIN)
                    + (frac128 * i128::from(NS_PER_MIN)) / INTERVAL_SCALE;
                let n64 =
                    i64::try_from(n).map_err(|_| QueryError::failed("interval nanos overflow"))?;
                nanos = nanos
                    .checked_add(n64)
                    .ok_or_else(|| QueryError::failed("interval nanos overflow"))?;
            }
            IntervalUnit::Second => {
                let n = int128 * i128::from(NS_PER_SEC)
                    + (frac128 * i128::from(NS_PER_SEC)) / INTERVAL_SCALE;
                let n64 =
                    i64::try_from(n).map_err(|_| QueryError::failed("interval nanos overflow"))?;
                nanos = nanos
                    .checked_add(n64)
                    .ok_or_else(|| QueryError::failed("interval nanos overflow"))?;
            }
            IntervalUnit::Millisecond => {
                let n = int128 * i128::from(NS_PER_MILLI)
                    + (frac128 * i128::from(NS_PER_MILLI)) / INTERVAL_SCALE;
                let n64 =
                    i64::try_from(n).map_err(|_| QueryError::failed("interval nanos overflow"))?;
                nanos = nanos
                    .checked_add(n64)
                    .ok_or_else(|| QueryError::failed("interval nanos overflow"))?;
            }
            IntervalUnit::Microsecond => {
                let n = int128 * i128::from(NS_PER_MICRO)
                    + (frac128 * i128::from(NS_PER_MICRO)) / INTERVAL_SCALE;
                let n64 =
                    i64::try_from(n).map_err(|_| QueryError::failed("interval nanos overflow"))?;
                nanos = nanos
                    .checked_add(n64)
                    .ok_or_else(|| QueryError::failed("interval nanos overflow"))?;
            }
            IntervalUnit::Nanosecond => {
                nanos = nanos
                    .checked_add(amount.integer)
                    .ok_or_else(|| QueryError::failed("interval nanos overflow"))?;
            }
        }
    }

    Ok(Interval {
        months,
        days,
        nanos,
    })
}

/// Adds or subtracts an [`Interval`] to/from a UTC nanosecond timestamp using exact calendar
/// month arithmetic (clamping end-of-month, e.g. Jan 31 + 1 month -> Feb 28/29), followed by
/// checked day and nanosecond arithmetic.
pub fn add_interval(ts_ns: i64, interval: Interval, subtract: bool) -> Result<i64, QueryError> {
    let after_months = if interval.months != 0 {
        let dt = DateTime::from_timestamp_nanos(ts_ns);
        let abs_months = interval.months.unsigned_abs();
        let add_positive = (interval.months > 0) ^ subtract;
        let shifted = if add_positive {
            dt.checked_add_months(Months::new(abs_months))
        } else {
            dt.checked_sub_months(Months::new(abs_months))
        }
        .ok_or_else(|| QueryError::failed("interval calendar addition overflow"))?;
        shifted
            .timestamp_nanos_opt()
            .ok_or_else(|| QueryError::failed("interval calendar addition overflow"))?
    } else {
        ts_ns
    };

    let day_ns = i128::from(interval.days) * i128::from(NS_PER_DAY);
    let sub_day_ns = day_ns + i128::from(interval.nanos);
    let delta = if subtract { -sub_day_ns } else { sub_day_ns };
    i64::try_from(i128::from(after_months) + delta)
        .map_err(|_| QueryError::failed("interval addition overflow"))
}

// ---------------------------------------------------------------------------
// Timestamp & date parsing / formatting
// ---------------------------------------------------------------------------

fn parse_ascii_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut acc: u32 = 0;
    for &b in bytes {
        acc = acc.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
    }
    Some(acc)
}

fn parse_ymd_prefix(bytes: &[u8]) -> Option<NaiveDate> {
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year = i32::try_from(parse_ascii_u32(&bytes[0..4])?).ok()?;
    let month = parse_ascii_u32(&bytes[5..7])?;
    let day = parse_ascii_u32(&bytes[8..10])?;
    NaiveDate::from_ymd_opt(year, month, day)
}

fn parse_hms_frac(bytes: &[u8]) -> Option<(NaiveTime, usize)> {
    if bytes.len() < 8 || bytes[2] != b':' || bytes[5] != b':' {
        return None;
    }
    let hour = parse_ascii_u32(&bytes[0..2])?;
    let minute = parse_ascii_u32(&bytes[3..5])?;
    let second = parse_ascii_u32(&bytes[6..8])?;

    let (nanos, consumed) = if bytes.get(8) == Some(&b'.') {
        let rest = &bytes[9..];
        let digit_count = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        if digit_count == 0 {
            return None;
        }
        let mut ns: u32 = 0;
        let used = digit_count.min(9);
        for &b in &rest[..used] {
            ns = ns * 10 + u32::from(b - b'0');
        }
        ns *= 10u32.pow((9 - used) as u32);
        (ns, 9 + digit_count)
    } else {
        (0, 8)
    };

    let time = if second == 60 {
        NaiveTime::from_hms_nano_opt(hour, minute, 59, 1_000_000_000 + nanos)?
    } else {
        NaiveTime::from_hms_nano_opt(hour, minute, second, nanos)?
    };
    Some((time, consumed))
}

fn parse_tz_offset(rest: &str) -> Option<FixedOffset> {
    let bytes = rest.as_bytes();
    let sign = match *bytes.first()? {
        b'+' => 1i32,
        b'-' => -1i32,
        _ => return None,
    };
    let tail = &bytes[1..];
    let (hh, mm) = match tail.len() {
        2 => (parse_ascii_u32(tail)?, 0),
        4 => (parse_ascii_u32(&tail[0..2])?, parse_ascii_u32(&tail[2..4])?),
        5 if tail[2] == b':' => (parse_ascii_u32(&tail[0..2])?, parse_ascii_u32(&tail[3..5])?),
        _ => return None,
    };
    if hh > 23 || mm > 59 {
        return None;
    }
    let total_secs = sign * (i32::try_from(hh).ok()? * 3600 + i32::try_from(mm).ok()? * 60);
    FixedOffset::east_opt(total_secs)
}

/// Parses an RFC 3339, offset-less timestamp, or date-only string into UTC epoch nanoseconds.
///
/// Explicit offsets (`Z` or `+HH:MM`/`-HH:MM`) are always converted to UTC instants regardless of
/// `utc`. Offset-less timestamps and date-only strings are interpreted as UTC (midnight UTC for
/// date-only).
pub fn parse_timestamp_ns(text: &str, _utc: bool) -> Result<i64, QueryError> {
    let s = text.trim_ascii();
    let bytes = s.as_bytes();
    let date =
        parse_ymd_prefix(bytes).ok_or_else(|| QueryError::failed("invalid timestamp string"))?;

    if bytes.len() == 10 {
        let naive = date
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| QueryError::failed("invalid timestamp date"))?;
        return naive
            .and_utc()
            .timestamp_nanos_opt()
            .ok_or_else(|| QueryError::failed("timestamp out of nanosecond range"));
    }

    if !matches!(bytes[10], b'T' | b't' | b' ') {
        return Err(QueryError::failed("invalid timestamp separator"));
    }

    let (time, time_len) =
        parse_hms_frac(&bytes[11..]).ok_or_else(|| QueryError::failed("invalid timestamp time"))?;
    let naive = date.and_time(time);
    let tail = &s[11 + time_len..];

    if tail.is_empty() || tail.eq_ignore_ascii_case("z") {
        return naive
            .and_utc()
            .timestamp_nanos_opt()
            .ok_or_else(|| QueryError::failed("timestamp out of nanosecond range"));
    }

    let offset_str = tail.trim_ascii_start();
    let offset = parse_tz_offset(offset_str)
        .ok_or_else(|| QueryError::failed("invalid timestamp offset"))?;
    let dt = offset
        .from_local_datetime(&naive)
        .single()
        .ok_or_else(|| QueryError::failed("invalid timestamp offset"))?;
    dt.with_timezone(&Utc)
        .timestamp_nanos_opt()
        .ok_or_else(|| QueryError::failed("timestamp out of nanosecond range"))
}

/// Formats a UTC nanosecond timestamp as RFC 3339 (`YYYY-MM-DDTHH:MM:SS[.fff...]Z` when `utc`
/// is true, or `YYYY-MM-DDTHH:MM:SS[.fff...]` when `utc` is false), stripping trailing
/// fractional zeros.
pub fn format_timestamp_ns(ns: i64, utc: bool) -> String {
    let dt = DateTime::from_timestamp_nanos(ns);
    let mut out = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    );
    let sub = dt.nanosecond();
    if sub > 0 {
        let frac = format!("{sub:09}");
        let trimmed = frac.trim_end_matches('0');
        out.push('.');
        out.push_str(trimmed);
    }
    if utc {
        out.push('Z');
    }
    out
}

/// Parses `'YYYY-MM-DD'` into days since `1970-01-01`.
pub fn parse_date_days(text: &str) -> Result<i32, QueryError> {
    let s = text.trim_ascii();
    let mut parts = s.split('-');
    let (Some(ys), Some(ms), Some(ds), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(QueryError::failed("invalid date string"));
    };
    if ys.len() != 4 || ms.is_empty() || ms.len() > 2 || ds.is_empty() || ds.len() > 2 {
        return Err(QueryError::failed("invalid date string"));
    }
    let year = parse_ascii_u32(ys.as_bytes())
        .and_then(|v| i32::try_from(v).ok())
        .ok_or_else(|| QueryError::failed("invalid date string"))?;
    let month =
        parse_ascii_u32(ms.as_bytes()).ok_or_else(|| QueryError::failed("invalid date string"))?;
    let day =
        parse_ascii_u32(ds.as_bytes()).ok_or_else(|| QueryError::failed("invalid date string"))?;
    let date = NaiveDate::from_ymd_opt(year, month, day)
        .ok_or_else(|| QueryError::failed("invalid date string"))?;
    Ok(date.num_days_from_ce() - UNIX_EPOCH_CE_DAYS)
}

/// Howard Hinnant's `civil_from_days`: splits Unix epoch days into `(year, month, day)`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + DAYS_EPOCH_SHIFT;
    let era = z.div_euclid(DAYS_PER_ERA);
    let day_of_era = z.rem_euclid(DAYS_PER_ERA);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Howard Hinnant's `days_from_civil`: inverse of [`civil_from_days`].
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_index = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * DAYS_PER_ERA + day_of_era - DAYS_EPOCH_SHIFT
}

/// Formats days since `1970-01-01` as `'YYYY-MM-DD'`.
pub fn format_date_days(days: i32) -> String {
    let (year, month, day) = civil_from_days(i64::from(days));
    if year < 0 {
        format!("-{:04}-{month:02}-{day:02}", year.unsigned_abs())
    } else {
        format!("{year:04}-{month:02}-{day:02}")
    }
}

// ---------------------------------------------------------------------------
// extract_return_type & field extraction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExtractField {
    Year,
    IsoYear,
    Quarter,
    Month,
    Week,
    Day,
    Dow,
    IsoDow,
    Doy,
    Hour,
    Minute,
    Second,
    Millisecond,
    Microsecond,
    Nanosecond,
    Epoch,
}

impl ExtractField {
    fn parse(field: &str) -> Result<Self, QueryError> {
        let stripped = field
            .trim_ascii()
            .strip_prefix(|c| c == '\'' || c == '"')
            .and_then(|s| s.strip_suffix(|c| c == '\'' || c == '"'))
            .unwrap_or(field.trim_ascii());
        match stripped.to_ascii_lowercase().as_str() {
            "y" | "yr" | "yrs" | "year" | "years" => Ok(Self::Year),
            "isoyear" => Ok(Self::IsoYear),
            "qtr" | "quarter" | "quarters" => Ok(Self::Quarter),
            "mon" | "mons" | "month" | "months" => Ok(Self::Month),
            "w" | "week" | "weeks" => Ok(Self::Week),
            "d" | "day" | "days" => Ok(Self::Day),
            "dow" | "dayofweek" => Ok(Self::Dow),
            "isodow" => Ok(Self::IsoDow),
            "doy" | "dayofyear" => Ok(Self::Doy),
            "h" | "hr" | "hrs" | "hour" | "hours" => Ok(Self::Hour),
            "m" | "min" | "mins" | "minute" | "minutes" => Ok(Self::Minute),
            "s" | "sec" | "secs" | "second" | "seconds" => Ok(Self::Second),
            "ms" | "msec" | "msecs" | "msecond" | "mseconds" | "millisecond" | "milliseconds" => {
                Ok(Self::Millisecond)
            }
            "us" | "usec" | "usecs" | "usecond" | "useconds" | "microsecond" | "microseconds" => {
                Ok(Self::Microsecond)
            }
            "ns" | "nsec" | "nsecs" | "nsecond" | "nseconds" | "nanosecond" | "nanoseconds" => {
                Ok(Self::Nanosecond)
            }
            "epoch" => Ok(Self::Epoch),
            _ => Err(QueryError::failed("unsupported extract field")),
        }
    }
}

/// Validates an `EXTRACT` / `date_part` field name (case-insensitive, matching DataFusion 55.2)
/// and returns its static output [`DataType`] (`DataType::F64` for `epoch`, `DataType::I64` for all
/// integer fields).
pub fn extract_return_type(field: &str) -> Result<DataType, QueryError> {
    match ExtractField::parse(field)? {
        ExtractField::Epoch => Ok(DataType::F64),
        _ => Ok(DataType::I64),
    }
}

// ---------------------------------------------------------------------------
// date_trunc units
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TruncUnit {
    Nanosecond,
    Microsecond,
    Millisecond,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl TruncUnit {
    fn parse(s: &str) -> Result<Self, QueryError> {
        match s.trim_ascii().to_ascii_lowercase().as_str() {
            "nanosecond" => Ok(Self::Nanosecond),
            "microsecond" => Ok(Self::Microsecond),
            "millisecond" => Ok(Self::Millisecond),
            "second" => Ok(Self::Second),
            "minute" => Ok(Self::Minute),
            "hour" => Ok(Self::Hour),
            "day" => Ok(Self::Day),
            "week" => Ok(Self::Week),
            "month" => Ok(Self::Month),
            "quarter" => Ok(Self::Quarter),
            "year" => Ok(Self::Year),
            _ => Err(QueryError::failed("unsupported date_trunc unit")),
        }
    }
}

fn truncate_timestamp_ns(ns: i64, unit: TruncUnit) -> Result<i64, QueryError> {
    let out = match unit {
        TruncUnit::Nanosecond => Some(ns),
        TruncUnit::Microsecond => ns.checked_sub(ns.rem_euclid(NS_PER_MICRO)),
        TruncUnit::Millisecond => ns.checked_sub(ns.rem_euclid(NS_PER_MILLI)),
        TruncUnit::Second => ns.checked_sub(ns.rem_euclid(NS_PER_SEC)),
        TruncUnit::Minute => ns.checked_sub(ns.rem_euclid(NS_PER_MIN)),
        TruncUnit::Hour => ns.checked_sub(ns.rem_euclid(NS_PER_HOUR)),
        TruncUnit::Day => ns.div_euclid(NS_PER_DAY).checked_mul(NS_PER_DAY),
        TruncUnit::Week => {
            let days = ns.div_euclid(NS_PER_DAY);
            let monday_days = days - (days + 3).rem_euclid(7);
            monday_days.checked_mul(NS_PER_DAY)
        }
        TruncUnit::Month => {
            let days = ns.div_euclid(NS_PER_DAY);
            let (year, month, _) = civil_from_days(days);
            days_from_civil(year, month, 1).checked_mul(NS_PER_DAY)
        }
        TruncUnit::Quarter => {
            let days = ns.div_euclid(NS_PER_DAY);
            let (year, month, _) = civil_from_days(days);
            let q_month = 1 + 3 * ((month - 1) / 3);
            days_from_civil(year, q_month, 1).checked_mul(NS_PER_DAY)
        }
        TruncUnit::Year => {
            let days = ns.div_euclid(NS_PER_DAY);
            let (year, _, _) = civil_from_days(days);
            days_from_civil(year, 1, 1).checked_mul(NS_PER_DAY)
        }
    };
    out.ok_or_else(|| QueryError::failed("date_trunc timestamp overflow"))
}

// ---------------------------------------------------------------------------
// Vectorized evaluation entry point
// ---------------------------------------------------------------------------

/// Evaluates time and calendar scalar functions (`Now`, `DateTrunc`, `DateBin`, `Extract`,
/// `ToTimestamp`) over `args` of length `len`, skipping inactive rows under `active`, checking
/// `ctx.cancel`, and maintaining the exact `Reservation` postcondition.
pub fn eval_time_fn(
    func: ScalarFn,
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let refs: Vec<&Column> = args.iter().collect();
    eval_time_fn_refs(func, &refs, len, active, ctx, pool)
}

pub(crate) fn eval_time_fn_refs(
    func: ScalarFn,
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(&pool);
    match eval_time_fn_inner(func, args, len, active, ctx, &mut pool, before) {
        Ok(col) => {
            reconcile_output(&mut pool, before, col.memory_bytes())?;
            Ok(col)
        }
        Err(err) => {
            rollback_to(&mut pool, before);
            Err(err)
        }
    }
}

fn eval_time_fn_inner(
    func: ScalarFn,
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    check_cancel(ctx, pool, before)?;
    if let Some(bm) = active
        && bm.len() != len
    {
        return Err(ColumnError::InvalidBatch {
            reason: "active bitmap length mismatch",
        }
        .into());
    }
    for &col in args {
        if col.len() != len {
            return Err(ColumnError::InvalidBatch {
                reason: "argument column length mismatch",
            }
            .into());
        }
    }

    match func {
        ScalarFn::Now => eval_now(args, len, active, ctx, pool, before),
        ScalarFn::DateTrunc => eval_date_trunc(args, len, active, ctx, pool, before),
        ScalarFn::DateBin => eval_date_bin(args, len, active, ctx, pool, before),
        ScalarFn::Extract => eval_extract(args, len, active, ctx, pool, before),
        ScalarFn::ToTimestamp => eval_to_timestamp(args, len, active, ctx, pool, before),
        _ => Err(QueryError::failed("unsupported time scalar function")),
    }
}

fn eval_now(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    if !args.is_empty() {
        return Err(QueryError::failed("now() takes no arguments"));
    }
    match active {
        None => Ok(Column::Const {
            value: ScalarValue::TimestampNs {
                value: ctx.now_ns,
                utc: true,
            },
            len,
        }),
        Some(bm) if bm.all_zeros() => Ok(Column::Const {
            value: ScalarValue::Null(DataType::TimestampNs { utc: true }),
            len,
        }),
        Some(bm) if bm.all_ones() => Ok(Column::Const {
            value: ScalarValue::TimestampNs {
                value: ctx.now_ns,
                utc: true,
            },
            len,
        }),
        Some(_) => {
            try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;
            let mut values = vec![0i64; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (row, slot) in values.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if is_active(active, row) {
                    *slot = ctx.now_ns;
                    let _ = nulls.set(row, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::TimestampNs { values, utc: true },
                Some(nulls),
            )?))
        }
    }
}

fn eval_date_trunc(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    let &[unit_col, ts_col] = args else {
        return Err(QueryError::failed("date_trunc expects 2 arguments"));
    };
    let utc = match ts_col.data_type() {
        DataType::TimestampNs { utc } => utc,
        DataType::Null => false,
        _ => {
            return Err(QueryError::failed(
                "date_trunc second argument must be timestamp",
            ));
        }
    };
    if !matches!(unit_col.data_type(), DataType::Utf8 | DataType::Null) {
        return Err(QueryError::failed(
            "date_trunc first argument must be string",
        ));
    }

    if len == 0
        || active.is_some_and(Bitmap::all_zeros)
        || matches!(ts_col.data_type(), DataType::Null)
    {
        return Ok(Column::Const {
            value: ScalarValue::Null(DataType::TimestampNs { utc }),
            len,
        });
    }

    let const_unit_str = match unit_col {
        Column::Const {
            value: ScalarValue::Utf8(s),
            ..
        } => Some(Some(s.as_str())),
        Column::Const {
            value: ScalarValue::Null(_),
            ..
        } => Some(None),
        _ => None,
    };
    let mut cached_const_unit: Option<TruncUnit> = None;

    try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;
    let mut values = vec![0i64; len];
    let mut nulls = Bitmap::new_zeroed(len);

    for (row, slot) in values.iter_mut().enumerate() {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        let Some(ns) = read_timestamp(ts_col, row) else {
            continue;
        };
        let unit = match const_unit_str {
            Some(Some(s)) => match cached_const_unit {
                Some(u) => u,
                None => {
                    let parsed = TruncUnit::parse(s)?;
                    cached_const_unit = Some(parsed);
                    parsed
                }
            },
            Some(None) => return Err(QueryError::failed("date_trunc unit cannot be null")),
            None => {
                let Some(s) = read_str(unit_col, row) else {
                    continue;
                };
                TruncUnit::parse(s)?
            }
        };
        *slot = truncate_timestamp_ns(ns, unit)?;
        let _ = nulls.set(row, true);
    }

    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::TimestampNs { values, utc },
        Some(nulls),
    )?))
}

fn validate_bin_stride(iv: Interval) -> Result<i64, QueryError> {
    if iv.months != 0 {
        return Err(QueryError::failed(
            "date_bin stride cannot contain month intervals",
        ));
    }
    let total = i128::from(iv.days) * i128::from(NS_PER_DAY) + i128::from(iv.nanos);
    let stride =
        i64::try_from(total).map_err(|_| QueryError::failed("date_bin stride overflow"))?;
    if stride <= 0 {
        return Err(QueryError::failed("date_bin stride must be positive"));
    }
    Ok(stride)
}

fn bin_timestamp_ns(src_ns: i64, stride_ns: i64, origin_ns: i64) -> Result<i64, QueryError> {
    let diff = i128::from(src_ns) - i128::from(origin_ns);
    let rem = diff.rem_euclid(i128::from(stride_ns));
    i64::try_from(i128::from(src_ns) - rem)
        .map_err(|_| QueryError::failed("date_bin result timestamp overflow"))
}

fn eval_date_bin(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    let (stride_col, src_col, origin_col) = match *args {
        [stride, src] => (stride, src, None),
        [stride, src, origin] => (stride, src, Some(origin)),
        _ => return Err(QueryError::failed("date_bin expects 2 or 3 arguments")),
    };

    if !matches!(stride_col.data_type(), DataType::Interval | DataType::Null) {
        return Err(QueryError::failed("date_bin stride must be an interval"));
    }
    let utc = match src_col.data_type() {
        DataType::TimestampNs { utc } => utc,
        DataType::Null => true,
        _ => return Err(QueryError::failed("date_bin source must be a timestamp")),
    };
    if let Some(orig) = origin_col
        && !matches!(
            orig.data_type(),
            DataType::TimestampNs { .. } | DataType::Null
        )
    {
        return Err(QueryError::failed("date_bin origin must be a timestamp"));
    }

    if len == 0
        || active.is_some_and(Bitmap::all_zeros)
        || matches!(stride_col.data_type(), DataType::Null)
        || matches!(src_col.data_type(), DataType::Null)
        || origin_col.is_some_and(|o| matches!(o.data_type(), DataType::Null))
    {
        return Ok(Column::Const {
            value: ScalarValue::Null(DataType::TimestampNs { utc }),
            len,
        });
    }

    let const_stride_iv = match stride_col {
        Column::Const {
            value: ScalarValue::Interval(iv),
            ..
        } => Some(*iv),
        _ => None,
    };
    let mut cached_const_stride: Option<i64> = None;

    try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;
    let mut values = vec![0i64; len];
    let mut nulls = Bitmap::new_zeroed(len);

    for (row, slot) in values.iter_mut().enumerate() {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        let Some(src_ns) = read_timestamp(src_col, row) else {
            continue;
        };
        let origin_ns = match origin_col {
            Some(orig) => {
                let Some(o) = read_timestamp(orig, row) else {
                    continue;
                };
                o
            }
            None => 0,
        };
        let stride_ns = match const_stride_iv {
            Some(iv) => match cached_const_stride {
                Some(s) => s,
                None => {
                    let s = validate_bin_stride(iv)?;
                    cached_const_stride = Some(s);
                    s
                }
            },
            None => {
                let Some(stride_iv) = read_interval(stride_col, row) else {
                    continue;
                };
                validate_bin_stride(stride_iv)?
            }
        };
        *slot = bin_timestamp_ns(src_ns, stride_ns, origin_ns)?;
        let _ = nulls.set(row, true);
    }

    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::TimestampNs { values, utc },
        Some(nulls),
    )?))
}

fn extract_from_timestamp(ns: i64, field: ExtractField) -> i64 {
    let dt = DateTime::from_timestamp_nanos(ns);
    match field {
        ExtractField::Year => i64::from(dt.year()),
        ExtractField::IsoYear => i64::from(dt.iso_week().year()),
        ExtractField::Quarter => 1 + i64::from(dt.month0() / 3),
        ExtractField::Month => i64::from(dt.month()),
        ExtractField::Week => i64::from(dt.iso_week().week()),
        ExtractField::Day => i64::from(dt.day()),
        ExtractField::Dow => i64::from(dt.weekday().num_days_from_sunday()),
        ExtractField::IsoDow => i64::from(dt.weekday().num_days_from_monday() + 1),
        ExtractField::Doy => i64::from(dt.ordinal()),
        ExtractField::Hour => i64::from(dt.hour()),
        ExtractField::Minute => i64::from(dt.minute()),
        ExtractField::Second => i64::from(dt.second()),
        ExtractField::Millisecond => {
            i64::from(dt.second()) * 1_000 + i64::from(dt.nanosecond() / 1_000_000)
        }
        ExtractField::Microsecond => {
            i64::from(dt.second()) * 1_000_000 + i64::from(dt.nanosecond() / 1_000)
        }
        ExtractField::Nanosecond => {
            i64::from(dt.second()) * 1_000_000_000 + i64::from(dt.nanosecond())
        }
        ExtractField::Epoch => 0,
    }
}

fn extract_from_date(days: i32, field: ExtractField) -> Result<i64, QueryError> {
    let date = days
        .checked_add(UNIX_EPOCH_CE_DAYS)
        .and_then(NaiveDate::from_num_days_from_ce_opt)
        .ok_or_else(|| QueryError::failed("date out of range"))?;
    Ok(match field {
        ExtractField::Year => i64::from(date.year()),
        ExtractField::IsoYear => i64::from(date.iso_week().year()),
        ExtractField::Quarter => 1 + i64::from(date.month0() / 3),
        ExtractField::Month => i64::from(date.month()),
        ExtractField::Week => i64::from(date.iso_week().week()),
        ExtractField::Day => i64::from(date.day()),
        ExtractField::Dow => i64::from(date.weekday().num_days_from_sunday()),
        ExtractField::IsoDow => i64::from(date.weekday().num_days_from_monday() + 1),
        ExtractField::Doy => i64::from(date.ordinal()),
        ExtractField::Hour
        | ExtractField::Minute
        | ExtractField::Second
        | ExtractField::Millisecond
        | ExtractField::Microsecond
        | ExtractField::Nanosecond
        | ExtractField::Epoch => 0,
    })
}

fn extract_from_interval(iv: Interval, field: ExtractField) -> Result<i64, QueryError> {
    match field {
        ExtractField::Year => Ok(i64::from(iv.months / 12)),
        ExtractField::Month => Ok(i64::from(iv.months % 12)),
        ExtractField::Week => Ok(i64::from(iv.days / 7)),
        ExtractField::Day => Ok(i64::from(iv.days)),
        ExtractField::Hour => Ok(iv.nanos / NS_PER_HOUR),
        ExtractField::Minute => Ok((iv.nanos / NS_PER_MIN) % 60),
        ExtractField::Second => Ok((iv.nanos / NS_PER_SEC) % 60),
        ExtractField::Millisecond => Ok((iv.nanos % NS_PER_MIN) / NS_PER_MILLI),
        ExtractField::Microsecond => Ok((iv.nanos % NS_PER_MIN) / NS_PER_MICRO),
        ExtractField::Nanosecond => Ok(iv.nanos % NS_PER_MIN),
        ExtractField::Quarter
        | ExtractField::IsoYear
        | ExtractField::Dow
        | ExtractField::IsoDow
        | ExtractField::Doy => Err(QueryError::failed("unsupported extract field for interval")),
        ExtractField::Epoch => Ok(0),
    }
}

fn eval_extract(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    let &[field_col, src_col] = args else {
        return Err(QueryError::failed("extract expects 2 arguments"));
    };

    let Column::Const {
        value: ScalarValue::Utf8(field_str),
        ..
    } = field_col
    else {
        return Err(QueryError::failed(
            "extract field must be a constant non-null string",
        ));
    };

    let field = ExtractField::parse(field_str)?;
    let ret_dt = if field == ExtractField::Epoch {
        DataType::F64
    } else {
        DataType::I64
    };

    if active.is_some_and(Bitmap::all_zeros) || matches!(src_col.data_type(), DataType::Null) {
        return Ok(Column::Const {
            value: ScalarValue::Null(ret_dt),
            len,
        });
    }

    let src_dt = src_col.data_type();
    if !matches!(
        src_dt,
        DataType::TimestampNs { .. } | DataType::Date | DataType::Interval
    ) {
        return Err(QueryError::failed(
            "extract source must be timestamp, date, or interval",
        ));
    }

    if matches!(src_dt, DataType::Interval)
        && matches!(
            field,
            ExtractField::Quarter
                | ExtractField::IsoYear
                | ExtractField::Dow
                | ExtractField::IsoDow
                | ExtractField::Doy
        )
    {
        return Err(QueryError::failed("unsupported extract field for interval"));
    }

    try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;

    if field == ExtractField::Epoch {
        let mut values = vec![0.0f64; len];
        let mut nulls = Bitmap::new_zeroed(len);
        for (row, slot) in values.iter_mut().enumerate() {
            if (row & CANCEL_CHECK_MASK) == 0 {
                check_cancel(ctx, pool, before)?;
            }
            if !is_active(active, row) {
                continue;
            }
            let val = match src_dt {
                DataType::TimestampNs { .. } => {
                    read_timestamp(src_col, row).map(|ns| (ns as f64) / 1_000_000_000.0)
                }
                DataType::Date => read_date(src_col, row).map(|d| f64::from(d) * 86_400.0),
                DataType::Interval => read_interval(src_col, row).map(|iv| {
                    f64::from(iv.months) * 30.0 * 86_400.0
                        + f64::from(iv.days) * 86_400.0
                        + (iv.nanos as f64) / 1_000_000_000.0
                }),
                _ => None,
            };
            if let Some(v) = val {
                *slot = v;
                let _ = nulls.set(row, true);
            }
        }
        Ok(Column::Flat(FlatColumn::try_new(
            FlatData::F64(values),
            Some(nulls),
        )?))
    } else {
        let mut values = vec![0i64; len];
        let mut nulls = Bitmap::new_zeroed(len);
        for (row, slot) in values.iter_mut().enumerate() {
            if (row & CANCEL_CHECK_MASK) == 0 {
                check_cancel(ctx, pool, before)?;
            }
            if !is_active(active, row) {
                continue;
            }
            let val = match src_dt {
                DataType::TimestampNs { .. } => {
                    read_timestamp(src_col, row).map(|ns| extract_from_timestamp(ns, field))
                }
                DataType::Date => match read_date(src_col, row) {
                    Some(d) => Some(extract_from_date(d, field)?),
                    None => None,
                },
                DataType::Interval => match read_interval(src_col, row) {
                    Some(iv) => Some(extract_from_interval(iv, field)?),
                    None => None,
                },
                _ => None,
            };
            if let Some(v) = val {
                *slot = v;
                let _ = nulls.set(row, true);
            }
        }
        Ok(Column::Flat(FlatColumn::try_new(
            FlatData::I64(values),
            Some(nulls),
        )?))
    }
}

fn eval_to_timestamp(
    args: &[&Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    let &[val_col] = args else {
        return Err(QueryError::failed("to_timestamp expects 1 argument"));
    };
    let dt = val_col.data_type();
    if active.is_some_and(Bitmap::all_zeros) || matches!(dt, DataType::Null) {
        return Ok(Column::Const {
            value: ScalarValue::Null(DataType::TimestampNs { utc: true }),
            len,
        });
    }
    if !matches!(
        dt,
        DataType::I64 | DataType::F64 | DataType::Utf8 | DataType::TimestampNs { .. }
    ) {
        return Err(QueryError::failed("unsupported type for to_timestamp"));
    }

    try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;
    let mut values = vec![0i64; len];
    let mut nulls = Bitmap::new_zeroed(len);

    for (row, slot) in values.iter_mut().enumerate() {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        let ts_ns = match dt {
            DataType::I64 => match read_i64(val_col, row) {
                Some(secs) => Some(
                    secs.checked_mul(NS_PER_SEC)
                        .ok_or_else(|| QueryError::failed("to_timestamp integer overflow"))?,
                ),
                None => None,
            },
            DataType::F64 => match read_f64(val_col, row) {
                Some(secs) => {
                    if !secs.is_finite() {
                        return Err(QueryError::failed(
                            "cannot convert non-finite float to timestamp",
                        ));
                    }
                    let scaled = (secs * 1_000_000_000.0).trunc();
                    if !scaled.is_finite()
                        || scaled < (i64::MIN as f64)
                        || scaled >= 9223372036854775808.0
                    {
                        return Err(QueryError::failed("to_timestamp float overflow"));
                    }
                    Some(scaled as i64)
                }
                None => None,
            },
            DataType::Utf8 => match read_str(val_col, row) {
                Some(s) => Some(parse_timestamp_ns(s, true)?),
                None => None,
            },
            DataType::TimestampNs { .. } => read_timestamp(val_col, row),
            _ => None,
        };
        if let Some(ns) = ts_ns {
            *slot = ns;
            let _ = nulls.set(row, true);
        }
    }

    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::TimestampNs { values, utc: true },
        Some(nulls),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cetra_column::{CancelToken, DictColumn, GlobalPool, RleColumn, Utf8Data};

    fn ctx_at(now_ns: i64) -> EvalContext {
        EvalContext::new(now_ns, CancelToken::new())
    }

    #[test]
    fn parse_interval_and_calendar_arithmetic() {
        assert_eq!(
            parse_interval("1 hour").unwrap(),
            Interval {
                months: 0,
                days: 0,
                nanos: NS_PER_HOUR,
            }
        );
        assert_eq!(
            parse_interval("30 minutes").unwrap(),
            Interval {
                months: 0,
                days: 0,
                nanos: 30 * NS_PER_MIN,
            }
        );
        assert_eq!(
            parse_interval("7 days").unwrap(),
            Interval {
                months: 0,
                days: 7,
                nanos: 0,
            }
        );
        assert_eq!(
            parse_interval("1 month").unwrap(),
            Interval {
                months: 1,
                days: 0,
                nanos: 0,
            }
        );
        assert_eq!(
            parse_interval("1 week").unwrap(),
            Interval {
                months: 0,
                days: 7,
                nanos: 0,
            }
        );
        assert_eq!(
            parse_interval("2 years 3 days 4 hours 5 seconds").unwrap(),
            Interval {
                months: 24,
                days: 3,
                nanos: 4 * NS_PER_HOUR + 5 * NS_PER_SEC,
            }
        );
        assert_eq!(
            parse_interval("500 milliseconds").unwrap(),
            Interval {
                months: 0,
                days: 0,
                nanos: 500 * NS_PER_MILLI,
            }
        );
        assert_eq!(
            parse_interval("100 microseconds").unwrap(),
            Interval {
                months: 0,
                days: 0,
                nanos: 100 * NS_PER_MICRO,
            }
        );
        assert_eq!(
            parse_interval("10 nanoseconds").unwrap(),
            Interval {
                months: 0,
                days: 0,
                nanos: 10,
            }
        );
        assert_eq!(
            parse_interval("1y 2mon 3w 4d 5h 6min 7sec 8ms 9us 10ns").unwrap(),
            Interval {
                months: 14,
                days: 25,
                nanos: 5 * NS_PER_HOUR
                    + 6 * NS_PER_MIN
                    + 7 * NS_PER_SEC
                    + 8 * NS_PER_MILLI
                    + 9 * NS_PER_MICRO
                    + 10,
            }
        );
        assert_eq!(
            parse_interval("-1.5 days").unwrap(),
            Interval {
                months: 0,
                days: -1,
                nanos: -12 * NS_PER_HOUR,
            }
        );
        assert_eq!(
            parse_interval("5 HOUR").unwrap(),
            Interval {
                months: 0,
                days: 0,
                nanos: 5 * NS_PER_HOUR,
            }
        );
        assert!(parse_interval("").is_err());
        assert!(parse_interval("1 lightyear").is_err());
        assert!(parse_interval("1 hour 2 hours").is_err());

        // Leap year & Jan 31 + 1 month clamping
        let jan31_non_leap = parse_timestamp_ns("2025-01-31T12:00:00Z", true).unwrap();
        let feb28 =
            add_interval(jan31_non_leap, parse_interval("1 month").unwrap(), false).unwrap();
        assert_eq!(format_timestamp_ns(feb28, true), "2025-02-28T12:00:00Z");

        let jan31_leap = parse_timestamp_ns("2024-01-31T12:00:00Z", true).unwrap();
        let feb29 = add_interval(jan31_leap, parse_interval("1 month").unwrap(), false).unwrap();
        assert_eq!(format_timestamp_ns(feb29, true), "2024-02-29T12:00:00Z");

        // Subtract negative interval
        let back = add_interval(feb29, parse_interval("-1 day").unwrap(), true).unwrap();
        assert_eq!(format_timestamp_ns(back, true), "2024-03-01T12:00:00Z");

        // Overflow -> Failed
        assert!(matches!(
            add_interval(
                i64::MAX,
                Interval {
                    months: 0,
                    days: 0,
                    nanos: 1,
                },
                false,
            )
            .unwrap_err(),
            QueryError::Failed(_)
        ));
        assert!(matches!(
            add_interval(
                i64::MIN,
                Interval {
                    months: 1,
                    days: 0,
                    nanos: 0,
                },
                true,
            )
            .unwrap_err(),
            QueryError::Failed(_)
        ));
    }

    #[test]
    fn date_trunc_and_date_bin_epoch_alignment() {
        let ts = parse_timestamp_ns("2026-01-08T14:25:36.123456789Z", true).unwrap();
        // 2026-01-08 is a Thursday; Monday of that ISO week is 2026-01-05.
        let expected_units = [
            ("nanosecond", "2026-01-08T14:25:36.123456789Z"),
            ("microsecond", "2026-01-08T14:25:36.123456Z"),
            ("millisecond", "2026-01-08T14:25:36.123Z"),
            ("second", "2026-01-08T14:25:36Z"),
            ("minute", "2026-01-08T14:25:00Z"),
            ("hour", "2026-01-08T14:00:00Z"),
            ("day", "2026-01-08T00:00:00Z"),
            ("week", "2026-01-05T00:00:00Z"),
            ("month", "2026-01-01T00:00:00Z"),
            ("quarter", "2026-01-01T00:00:00Z"),
            ("year", "2026-01-01T00:00:00Z"),
        ];
        for (unit, expected) in expected_units {
            let unit_col = Column::try_new_const(ScalarValue::Utf8(unit.into()), 1).unwrap();
            let ts_col = Column::try_new_const(
                ScalarValue::TimestampNs {
                    value: ts,
                    utc: true,
                },
                1,
            )
            .unwrap();
            let out = eval_time_fn(
                ScalarFn::DateTrunc,
                &[unit_col, ts_col],
                1,
                None,
                &ctx_at(0),
                None,
            )
            .unwrap();
            assert_eq!(out.data_type(), DataType::TimestampNs { utc: true });
            assert_eq!(
                format_timestamp_ns(read_timestamp(&out, 0).unwrap(), true),
                expected,
                "unit={unit}"
            );
        }

        // Quarter Q2/Q3/Q4 and utc=false preserved
        let q3_ts = parse_timestamp_ns("2026-08-19T09:10:11Z", false).unwrap();
        let unit_col = Column::try_new_const(ScalarValue::Utf8("QUARTER".into()), 1).unwrap();
        let ts_col = Column::try_new_const(
            ScalarValue::TimestampNs {
                value: q3_ts,
                utc: false,
            },
            1,
        )
        .unwrap();
        let q_out = eval_time_fn(
            ScalarFn::DateTrunc,
            &[unit_col, ts_col],
            1,
            None,
            &ctx_at(0),
            None,
        )
        .unwrap();
        assert_eq!(q_out.data_type(), DataType::TimestampNs { utc: false });
        assert_eq!(
            format_timestamp_ns(read_timestamp(&q_out, 0).unwrap(), false),
            "2026-07-01T00:00:00"
        );

        // Unknown unit -> Failed
        let bad_unit = Column::try_new_const(ScalarValue::Utf8("century".into()), 1).unwrap();
        let ts_col = Column::try_new_const(
            ScalarValue::TimestampNs {
                value: ts,
                utc: true,
            },
            1,
        )
        .unwrap();
        assert!(matches!(
            eval_time_fn(
                ScalarFn::DateTrunc,
                &[bad_unit, ts_col],
                1,
                None,
                &ctx_at(0),
                None,
            )
            .unwrap_err(),
            QueryError::Failed(_)
        ));

        // date_bin with default origin (epoch), explicit origin, and pre-1970 timestamps
        let pre_epoch = parse_timestamp_ns("1969-12-31T23:54:10Z", true).unwrap();
        let post_epoch = parse_timestamp_ns("2026-01-01T10:17:45Z", true).unwrap();
        let stride = Column::try_new_const(
            ScalarValue::Interval(parse_interval("10 minutes").unwrap()),
            2,
        )
        .unwrap();
        let src_flat = FlatColumn::try_new(
            FlatData::TimestampNs {
                values: vec![pre_epoch, post_epoch],
                utc: true,
            },
            None,
        )
        .unwrap();
        let src_rle = Column::Rle(RleColumn::try_new(src_flat, vec![1, 2]).unwrap());
        let binned = eval_time_fn(
            ScalarFn::DateBin,
            &[stride, src_rle],
            2,
            None,
            &ctx_at(0),
            None,
        )
        .unwrap();
        assert_eq!(
            format_timestamp_ns(read_timestamp(&binned, 0).unwrap(), true),
            "1969-12-31T23:50:00Z"
        );
        assert_eq!(
            format_timestamp_ns(read_timestamp(&binned, 1).unwrap(), true),
            "2026-01-01T10:10:00Z"
        );

        // Explicit origin shifted by +3 minutes
        let origin_ns = 3 * NS_PER_MIN;
        let stride2 = Column::try_new_const(
            ScalarValue::Interval(parse_interval("10 minutes").unwrap()),
            1,
        )
        .unwrap();
        let src2 = Column::try_new_const(
            ScalarValue::TimestampNs {
                value: post_epoch,
                utc: true,
            },
            1,
        )
        .unwrap();
        let orig2 = Column::try_new_const(
            ScalarValue::TimestampNs {
                value: origin_ns,
                utc: true,
            },
            1,
        )
        .unwrap();
        let binned2 = eval_time_fn(
            ScalarFn::DateBin,
            &[stride2, src2, orig2],
            1,
            None,
            &ctx_at(0),
            None,
        )
        .unwrap();
        assert_eq!(
            format_timestamp_ns(read_timestamp(&binned2, 0).unwrap(), true),
            "2026-01-01T10:13:00Z"
        );

        // Zero, negative, and month-bearing stride -> Failed
        for bad_iv in [
            Interval::default(),
            parse_interval("-5 minutes").unwrap(),
            parse_interval("1 month").unwrap(),
        ] {
            let s = Column::try_new_const(ScalarValue::Interval(bad_iv), 1).unwrap();
            let t = Column::try_new_const(
                ScalarValue::TimestampNs {
                    value: 0,
                    utc: true,
                },
                1,
            )
            .unwrap();
            assert!(matches!(
                eval_time_fn(ScalarFn::DateBin, &[s, t], 1, None, &ctx_at(0), None).unwrap_err(),
                QueryError::Failed(_)
            ));
        }
    }

    #[test]
    fn timestamp_parsing_and_formatting() {
        let a = parse_timestamp_ns("2026-01-01T10:00:03Z", true).unwrap();
        let b = parse_timestamp_ns("2026-01-01T12:00:03+02:00", true).unwrap();
        let c = parse_timestamp_ns(" 2026-01-01 10:00:03 ", false).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(format_timestamp_ns(a, true), "2026-01-01T10:00:03Z");
        assert_eq!(format_timestamp_ns(a, false), "2026-01-01T10:00:03");

        let d = parse_timestamp_ns("2026-01-01", true).unwrap();
        assert_eq!(format_timestamp_ns(d, true), "2026-01-01T00:00:00Z");

        let frac = parse_timestamp_ns("2026-01-01T10:00:03.123000Z", true).unwrap();
        assert_eq!(format_timestamp_ns(frac, true), "2026-01-01T10:00:03.123Z");

        // Pre-1970 round-trip
        let pre = parse_timestamp_ns("1969-12-31T23:59:59.5Z", true).unwrap();
        assert_eq!(pre, -500_000_000);
        assert_eq!(format_timestamp_ns(pre, true), "1969-12-31T23:59:59.5Z");

        assert!(matches!(
            parse_timestamp_ns("not-a-timestamp", true).unwrap_err(),
            QueryError::Failed(_)
        ));
        assert!(matches!(
            parse_timestamp_ns("2026-02-30T00:00:00Z", true).unwrap_err(),
            QueryError::Failed(_)
        ));

        let days = parse_date_days("2026-01-01").unwrap();
        assert_eq!(format_date_days(days), "2026-01-01");
        assert_eq!(format_date_days(0), "1970-01-01");
        assert_eq!(format_date_days(-1), "1969-12-31");
        assert!(parse_date_days("2026-13-01").is_err());

        // ToTimestamp across I64, F64, Utf8 (including Dict encoding)
        let i_col = Column::try_new_const(ScalarValue::I64(1_767_261_603), 1).unwrap();
        let i_out =
            eval_time_fn(ScalarFn::ToTimestamp, &[i_col], 1, None, &ctx_at(0), None).unwrap();
        assert_eq!(
            format_timestamp_ns(read_timestamp(&i_out, 0).unwrap(), true),
            "2026-01-01T10:00:03Z"
        );

        let f_col = Column::try_new_const(ScalarValue::F64(1.5), 1).unwrap();
        let f_out =
            eval_time_fn(ScalarFn::ToTimestamp, &[f_col], 1, None, &ctx_at(0), None).unwrap();
        assert_eq!(read_timestamp(&f_out, 0), Some(1_500_000_000));

        let bad_f = Column::try_new_const(ScalarValue::F64(f64::NAN), 1).unwrap();
        assert!(matches!(
            eval_time_fn(ScalarFn::ToTimestamp, &[bad_f], 1, None, &ctx_at(0), None).unwrap_err(),
            QueryError::Failed(_)
        ));

        let utf8_vals = FlatColumn::try_new(
            FlatData::Utf8(
                Utf8Data::try_from_slices(["2026-01-01T10:00:03Z", "2026-01-02"]).unwrap(),
            ),
            None,
        )
        .unwrap();
        let dict_col = Column::Dict(DictColumn::try_new(utf8_vals, vec![1, 0], None).unwrap());
        let s_out = eval_time_fn(
            ScalarFn::ToTimestamp,
            &[dict_col],
            2,
            None,
            &ctx_at(0),
            None,
        )
        .unwrap();
        assert_eq!(
            format_timestamp_ns(read_timestamp(&s_out, 0).unwrap(), true),
            "2026-01-02T00:00:00Z"
        );
        assert_eq!(
            format_timestamp_ns(read_timestamp(&s_out, 1).unwrap(), true),
            "2026-01-01T10:00:03Z"
        );
    }

    #[test]
    fn extract_fields_types_and_now_uses_ctx() {
        // Now with args = &[] and len = 5
        let now_col = eval_time_fn(ScalarFn::Now, &[], 5, None, &ctx_at(42_000), None).unwrap();
        assert_eq!(now_col.len(), 5);
        assert_eq!(now_col.data_type(), DataType::TimestampNs { utc: true });
        for row in 0..5 {
            assert_eq!(read_timestamp(&now_col, row), Some(42_000));
        }

        // extract_return_type
        for field in [
            "year",
            "quarter",
            "month",
            "week",
            "day",
            "dow",
            "doy",
            "hour",
            "minute",
            "second",
            "millisecond",
            "microsecond",
            "nanosecond",
        ] {
            assert_eq!(extract_return_type(field).unwrap(), DataType::I64);
        }
        assert_eq!(extract_return_type("EPOCH").unwrap(), DataType::F64);
        assert!(extract_return_type("century").is_err());

        // TimestampNs extraction (2026-01-04 is Sunday -> dow = 0)
        let ts = parse_timestamp_ns("2026-01-04T15:04:05.123456789Z", true).unwrap();
        let ts_col = Column::try_new_const(
            ScalarValue::TimestampNs {
                value: ts,
                utc: true,
            },
            1,
        )
        .unwrap();
        let ts_checks = [
            ("year", 2026i64),
            ("quarter", 1),
            ("month", 1),
            ("week", 1),
            ("day", 4),
            ("dow", 0),
            ("doy", 4),
            ("hour", 15),
            ("minute", 4),
            ("second", 5),
            ("millisecond", 5_123),
            ("microsecond", 5_123_456),
            ("nanosecond", 5_123_456_789),
        ];
        for (field, expected) in ts_checks {
            let f_col = Column::try_new_const(ScalarValue::Utf8(field.into()), 1).unwrap();
            let out = eval_time_fn(
                ScalarFn::Extract,
                &[f_col, ts_col.clone()],
                1,
                None,
                &ctx_at(0),
                None,
            )
            .unwrap();
            assert_eq!(out.data_type(), DataType::I64);
            assert_eq!(read_i64(&out, 0), Some(expected), "field={field}");
        }

        let epoch_f = Column::try_new_const(ScalarValue::Utf8("epoch".into()), 1).unwrap();
        let epoch_out = eval_time_fn(
            ScalarFn::Extract,
            &[epoch_f, ts_col],
            1,
            None,
            &ctx_at(0),
            None,
        )
        .unwrap();
        assert_eq!(epoch_out.data_type(), DataType::F64);
        assert!((read_f64(&epoch_out, 0).unwrap() - (ts as f64 / 1e9)).abs() < 1e-6);

        // Date source (midnight UTC)
        let d_days = parse_date_days("2026-07-04").unwrap();
        let d_col = Column::try_new_const(ScalarValue::Date(d_days), 1).unwrap();
        let q_f = Column::try_new_const(ScalarValue::Utf8("quarter".into()), 1).unwrap();
        let q_out = eval_time_fn(
            ScalarFn::Extract,
            &[q_f, d_col.clone()],
            1,
            None,
            &ctx_at(0),
            None,
        )
        .unwrap();
        assert_eq!(read_i64(&q_out, 0), Some(3));
        let h_f = Column::try_new_const(ScalarValue::Utf8("hour".into()), 1).unwrap();
        let h_out = eval_time_fn(
            ScalarFn::Extract,
            &[h_f, d_col.clone()],
            1,
            None,
            &ctx_at(0),
            None,
        )
        .unwrap();
        assert_eq!(read_i64(&h_out, 0), Some(0));
        let de_f = Column::try_new_const(ScalarValue::Utf8("epoch".into()), 1).unwrap();
        let de_out =
            eval_time_fn(ScalarFn::Extract, &[de_f, d_col], 1, None, &ctx_at(0), None).unwrap();
        assert_eq!(read_f64(&de_out, 0), Some(f64::from(d_days) * 86_400.0));

        // Interval source
        let iv = Interval {
            months: 14,
            days: 10,
            nanos: 3 * NS_PER_HOUR + 4 * NS_PER_MIN + 5 * NS_PER_SEC + 6 * NS_PER_MILLI,
        };
        let iv_col = Column::try_new_const(ScalarValue::Interval(iv), 1).unwrap();
        for (field, expected) in [
            ("year", 1i64),
            ("month", 2),
            ("week", 1),
            ("day", 10),
            ("hour", 3),
            ("minute", 4),
            ("second", 5),
            ("millisecond", 5_006),
        ] {
            let f_col = Column::try_new_const(ScalarValue::Utf8(field.into()), 1).unwrap();
            let out = eval_time_fn(
                ScalarFn::Extract,
                &[f_col, iv_col.clone()],
                1,
                None,
                &ctx_at(0),
                None,
            )
            .unwrap();
            assert_eq!(read_i64(&out, 0), Some(expected), "iv field={field}");
        }

        // Non-constant field rejected with Failed
        let non_const_f = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["year"]).unwrap()),
                None,
            )
            .unwrap(),
        );
        assert!(matches!(
            eval_time_fn(
                ScalarFn::Extract,
                &[non_const_f, iv_col],
                1,
                None,
                &ctx_at(0),
                None,
            )
            .unwrap_err(),
            QueryError::Failed(_)
        ));

        // Inactive rows with invalid stride / invalid timestamp string do not fault
        let active = Bitmap::from_bools(&[true, false]);
        let bad_strides = Column::Flat(
            FlatColumn::try_new(
                FlatData::Interval(vec![
                    parse_interval("5 minutes").unwrap(),
                    Interval::default(), // invalid 0 stride on inactive row 1
                ]),
                None,
            )
            .unwrap(),
        );
        let ts_pair = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vec![300 * NS_PER_SEC, 0],
                    utc: true,
                },
                None,
            )
            .unwrap(),
        );
        let masked_bin = eval_time_fn(
            ScalarFn::DateBin,
            &[bad_strides, ts_pair],
            2,
            Some(&active),
            &ctx_at(0),
            None,
        )
        .unwrap();
        assert_eq!(read_timestamp(&masked_bin, 0), Some(300 * NS_PER_SEC));
        assert!(masked_bin.is_null(1));

        let bad_strs = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(["2026-01-01T00:00:00Z", "not-a-ts"]).unwrap(),
                ),
                None,
            )
            .unwrap(),
        );
        let masked_ts = eval_time_fn(
            ScalarFn::ToTimestamp,
            std::slice::from_ref(&bad_strs),
            2,
            Some(&active),
            &ctx_at(0),
            None,
        )
        .unwrap();
        assert_eq!(
            format_timestamp_ns(read_timestamp(&masked_ts, 0).unwrap(), true),
            "2026-01-01T00:00:00Z"
        );
        assert!(masked_ts.is_null(1));

        // Reservation postcondition: Ok -> before + col.memory_bytes(), Err -> before
        let scope = GlobalPool::standalone(256);
        let mut res = scope.register("time", false);
        res.try_grow(32).unwrap();
        let before = res.size();

        let ok_col = eval_time_fn(
            ScalarFn::ToTimestamp,
            &[bad_strs],
            2,
            Some(&active),
            &ctx_at(0),
            Some(&mut res),
        )
        .unwrap();
        assert_eq!(res.size(), before + ok_col.memory_bytes());

        // Tight OOM rollback
        let big_in = Column::try_new_const(ScalarValue::I64(100), 64).unwrap();
        let oom_err = eval_time_fn(
            ScalarFn::ToTimestamp,
            &[big_in],
            64,
            None,
            &ctx_at(0),
            Some(&mut res),
        )
        .unwrap_err();
        assert!(matches!(oom_err, QueryError::TooLarge));
        assert_eq!(res.size(), before + ok_col.memory_bytes());

        // CancelToken aborts cleanly and rolls back reservation
        let cancelled = ctx_at(0);
        cancelled.cancel.cancel();
        let cancel_in = Column::try_new_const(ScalarValue::I64(100), 4).unwrap();
        let cancel_err = eval_time_fn(
            ScalarFn::ToTimestamp,
            &[cancel_in],
            4,
            None,
            &cancelled,
            Some(&mut res),
        )
        .unwrap_err();
        assert!(matches!(cancel_err, QueryError::Cancelled));
        assert_eq!(res.size(), before + ok_col.memory_bytes());
    }

    #[test]
    fn date_trunc_and_date_bin_empty_and_all_null_do_not_fault() {
        let bad_unit = Column::try_new_const(ScalarValue::Utf8("century".into()), 0).unwrap();
        let empty_ts = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vec![],
                    utc: true,
                },
                None,
            )
            .unwrap(),
        );
        let empty_trunc = eval_time_fn(
            ScalarFn::DateTrunc,
            &[bad_unit, empty_ts.clone()],
            0,
            None,
            &ctx_at(0),
            None,
        )
        .unwrap();
        assert_eq!(empty_trunc.len(), 0);

        let all_null_ts = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vec![0, 0],
                    utc: true,
                },
                Some(Bitmap::from_bools(&[false, false])),
            )
            .unwrap(),
        );
        let bad_unit2 = Column::try_new_const(ScalarValue::Utf8("century".into()), 2).unwrap();
        let null_trunc = eval_time_fn(
            ScalarFn::DateTrunc,
            &[bad_unit2, all_null_ts.clone()],
            2,
            None,
            &ctx_at(0),
            None,
        )
        .unwrap();
        assert!(null_trunc.is_null(0));
        assert!(null_trunc.is_null(1));

        let null_unit = Column::try_new_const(ScalarValue::Null(DataType::Utf8), 2).unwrap();
        let null_unit_trunc = eval_time_fn(
            ScalarFn::DateTrunc,
            &[null_unit, all_null_ts.clone()],
            2,
            None,
            &ctx_at(0),
            None,
        )
        .unwrap();
        assert!(null_unit_trunc.is_null(0));

        // date_bin with invalid Const stride on len == 0 or all-NULL src_col must not fault.
        let bad_stride0 =
            Column::try_new_const(ScalarValue::Interval(Interval::default()), 0).unwrap();
        let empty_bin = eval_time_fn(
            ScalarFn::DateBin,
            &[bad_stride0, empty_ts],
            0,
            None,
            &ctx_at(0),
            None,
        )
        .unwrap();
        assert_eq!(empty_bin.len(), 0);

        let month_stride =
            Column::try_new_const(ScalarValue::Interval(parse_interval("1 month").unwrap()), 2)
                .unwrap();
        let null_bin = eval_time_fn(
            ScalarFn::DateBin,
            &[month_stride, all_null_ts],
            2,
            None,
            &ctx_at(0),
            None,
        )
        .unwrap();
        assert!(null_bin.is_null(0));
        assert!(null_bin.is_null(1));
    }
}
