//! Buckets for the counts and durations that leave the process (D-TL-8): never an exact count. The serde strings are the wire format; changing one changes the heartbeat.

use std::time::Duration;

use serde::Serialize;

/// A count of orgs, projects or users.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum CountBucket {
    #[serde(rename = "0")]
    Zero,
    #[serde(rename = "1")]
    One,
    #[serde(rename = "2-5")]
    TwoToFive,
    #[serde(rename = "6-20")]
    SixToTwenty,
    #[serde(rename = "21-100")]
    TwentyOneToHundred,
    #[serde(rename = "101-1000")]
    HundredOneToThousand,
    /// More than 1000.
    #[serde(rename = "1000+")]
    OverThousand,
}

/// `n` as a [`CountBucket`].
pub(crate) fn count_bucket(n: u64) -> CountBucket {
    match n {
        0 => CountBucket::Zero,
        1 => CountBucket::One,
        2..=5 => CountBucket::TwoToFive,
        6..=20 => CountBucket::SixToTwenty,
        21..=100 => CountBucket::TwentyOneToHundred,
        101..=1000 => CountBucket::HundredOneToThousand,
        _ => CountBucket::OverThousand,
    }
}

/// A daily volume of spans or scores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum VolumeBucket {
    #[serde(rename = "0")]
    Zero,
    #[serde(rename = "<1k")]
    UnderThousand,
    #[serde(rename = "<10k")]
    UnderTenThousand,
    #[serde(rename = "<100k")]
    UnderHundredThousand,
    #[serde(rename = "<1M")]
    UnderMillion,
    #[serde(rename = "<10M")]
    UnderTenMillion,
    #[serde(rename = "<100M")]
    UnderHundredMillion,
    #[serde(rename = "100M+")]
    HundredMillionOrMore,
}

/// `n` as a [`VolumeBucket`].
pub(crate) fn volume_bucket(n: u64) -> VolumeBucket {
    match n {
        0 => VolumeBucket::Zero,
        1..=999 => VolumeBucket::UnderThousand,
        1_000..=9_999 => VolumeBucket::UnderTenThousand,
        10_000..=99_999 => VolumeBucket::UnderHundredThousand,
        100_000..=999_999 => VolumeBucket::UnderMillion,
        1_000_000..=9_999_999 => VolumeBucket::UnderTenMillion,
        10_000_000..=99_999_999 => VolumeBucket::UnderHundredMillion,
        _ => VolumeBucket::HundredMillionOrMore,
    }
}

const HOUR: u64 = 3_600;
const DAY: u64 = 24 * HOUR;

/// How long the process has run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum UptimeBucket {
    #[serde(rename = "<1h")]
    UnderHour,
    #[serde(rename = "1h-1d")]
    HourToDay,
    #[serde(rename = "1d-7d")]
    DayToWeek,
    #[serde(rename = "7d-30d")]
    WeekToMonth,
    #[serde(rename = "30d+")]
    MonthOrMore,
}

/// `uptime` as an [`UptimeBucket`]. Each lower bound is inclusive.
pub(crate) fn uptime_bucket(uptime: Duration) -> UptimeBucket {
    match uptime.as_secs() {
        s if s < HOUR => UptimeBucket::UnderHour,
        s if s < DAY => UptimeBucket::HourToDay,
        s if s < 7 * DAY => UptimeBucket::DayToWeek,
        s if s < 30 * DAY => UptimeBucket::WeekToMonth,
        _ => UptimeBucket::MonthOrMore,
    }
}

/// How long ago the instance was set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum InstallAgeBucket {
    #[serde(rename = "<1d")]
    UnderDay,
    #[serde(rename = "1d-7d")]
    DayToWeek,
    #[serde(rename = "7d-30d")]
    WeekToMonth,
    #[serde(rename = "30d-90d")]
    MonthToQuarter,
    #[serde(rename = "90d-1y")]
    QuarterToYear,
    #[serde(rename = "1y+")]
    YearOrMore,
}

/// `age` as an [`InstallAgeBucket`]. Each lower bound is inclusive; a year is 365 days.
pub(crate) fn install_age_bucket(age: Duration) -> InstallAgeBucket {
    match age.as_secs() {
        s if s < DAY => InstallAgeBucket::UnderDay,
        s if s < 7 * DAY => InstallAgeBucket::DayToWeek,
        s if s < 30 * DAY => InstallAgeBucket::WeekToMonth,
        s if s < 90 * DAY => InstallAgeBucket::MonthToQuarter,
        s if s < 365 * DAY => InstallAgeBucket::QuarterToYear,
        _ => InstallAgeBucket::YearOrMore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_bucket_edges() {
        assert_eq!(count_bucket(0), CountBucket::Zero);
        assert_eq!(count_bucket(1), CountBucket::One);
        assert_eq!(count_bucket(2), CountBucket::TwoToFive);
        assert_eq!(count_bucket(5), CountBucket::TwoToFive);
        assert_eq!(count_bucket(6), CountBucket::SixToTwenty);
        assert_eq!(count_bucket(20), CountBucket::SixToTwenty);
        assert_eq!(count_bucket(21), CountBucket::TwentyOneToHundred);
        assert_eq!(count_bucket(100), CountBucket::TwentyOneToHundred);
        assert_eq!(count_bucket(101), CountBucket::HundredOneToThousand);
        assert_eq!(count_bucket(1000), CountBucket::HundredOneToThousand);
        assert_eq!(count_bucket(1001), CountBucket::OverThousand);
        assert_eq!(count_bucket(u64::MAX), CountBucket::OverThousand);
    }

    #[test]
    fn volume_bucket_edges() {
        assert_eq!(volume_bucket(0), VolumeBucket::Zero);
        assert_eq!(volume_bucket(1), VolumeBucket::UnderThousand);
        assert_eq!(volume_bucket(999), VolumeBucket::UnderThousand);
        assert_eq!(volume_bucket(1_000), VolumeBucket::UnderTenThousand);
        assert_eq!(volume_bucket(9_999), VolumeBucket::UnderTenThousand);
        assert_eq!(volume_bucket(10_000), VolumeBucket::UnderHundredThousand);
        assert_eq!(volume_bucket(99_999), VolumeBucket::UnderHundredThousand);
        assert_eq!(volume_bucket(100_000), VolumeBucket::UnderMillion);
        assert_eq!(volume_bucket(999_999), VolumeBucket::UnderMillion);
        assert_eq!(volume_bucket(1_000_000), VolumeBucket::UnderTenMillion);
        assert_eq!(volume_bucket(9_999_999), VolumeBucket::UnderTenMillion);
        assert_eq!(volume_bucket(10_000_000), VolumeBucket::UnderHundredMillion);
        assert_eq!(volume_bucket(99_999_999), VolumeBucket::UnderHundredMillion);
        assert_eq!(
            volume_bucket(100_000_000),
            VolumeBucket::HundredMillionOrMore
        );
        assert_eq!(volume_bucket(u64::MAX), VolumeBucket::HundredMillionOrMore);
    }

    #[test]
    fn uptime_bucket_edges() {
        assert_eq!(
            uptime_bucket(Duration::from_secs(0)),
            UptimeBucket::UnderHour
        );
        assert_eq!(
            uptime_bucket(Duration::from_secs(3_599)),
            UptimeBucket::UnderHour
        );
        assert_eq!(
            uptime_bucket(Duration::from_secs(3_600)),
            UptimeBucket::HourToDay
        );
        assert_eq!(
            uptime_bucket(Duration::from_secs(86_399)),
            UptimeBucket::HourToDay
        );
        assert_eq!(
            uptime_bucket(Duration::from_secs(86_400)),
            UptimeBucket::DayToWeek
        );
        assert_eq!(
            uptime_bucket(Duration::from_secs(7 * DAY - 1)),
            UptimeBucket::DayToWeek
        );
        assert_eq!(
            uptime_bucket(Duration::from_secs(7 * DAY)),
            UptimeBucket::WeekToMonth
        );
        assert_eq!(
            uptime_bucket(Duration::from_secs(30 * DAY - 1)),
            UptimeBucket::WeekToMonth
        );
        assert_eq!(
            uptime_bucket(Duration::from_secs(30 * DAY)),
            UptimeBucket::MonthOrMore
        );
        assert_eq!(uptime_bucket(Duration::MAX), UptimeBucket::MonthOrMore);
    }

    #[test]
    fn install_age_bucket_edges() {
        assert_eq!(
            install_age_bucket(Duration::from_secs(0)),
            InstallAgeBucket::UnderDay
        );
        assert_eq!(
            install_age_bucket(Duration::from_secs(DAY - 1)),
            InstallAgeBucket::UnderDay
        );
        assert_eq!(
            install_age_bucket(Duration::from_secs(DAY)),
            InstallAgeBucket::DayToWeek
        );
        assert_eq!(
            install_age_bucket(Duration::from_secs(7 * DAY)),
            InstallAgeBucket::WeekToMonth
        );
        assert_eq!(
            install_age_bucket(Duration::from_secs(30 * DAY)),
            InstallAgeBucket::MonthToQuarter
        );
        assert_eq!(
            install_age_bucket(Duration::from_secs(90 * DAY - 1)),
            InstallAgeBucket::MonthToQuarter
        );
        assert_eq!(
            install_age_bucket(Duration::from_secs(90 * DAY)),
            InstallAgeBucket::QuarterToYear
        );
        assert_eq!(
            install_age_bucket(Duration::from_secs(365 * DAY - 1)),
            InstallAgeBucket::QuarterToYear
        );
        assert_eq!(
            install_age_bucket(Duration::from_secs(365 * DAY)),
            InstallAgeBucket::YearOrMore
        );
    }

    /// The JSON string of a value, for the wire-format test.
    fn wire<T: Serialize>(value: &T) -> String {
        serde_json::to_string(value).unwrap()
    }

    #[test]
    fn bucket_strings_are_the_wire_format() {
        assert_eq!(wire(&CountBucket::Zero), "\"0\"");
        assert_eq!(wire(&CountBucket::One), "\"1\"");
        assert_eq!(wire(&CountBucket::TwoToFive), "\"2-5\"");
        assert_eq!(wire(&CountBucket::SixToTwenty), "\"6-20\"");
        assert_eq!(wire(&CountBucket::TwentyOneToHundred), "\"21-100\"");
        assert_eq!(wire(&CountBucket::HundredOneToThousand), "\"101-1000\"");
        assert_eq!(wire(&CountBucket::OverThousand), "\"1000+\"");

        assert_eq!(wire(&VolumeBucket::Zero), "\"0\"");
        assert_eq!(wire(&VolumeBucket::UnderThousand), "\"<1k\"");
        assert_eq!(wire(&VolumeBucket::UnderTenThousand), "\"<10k\"");
        assert_eq!(wire(&VolumeBucket::UnderHundredThousand), "\"<100k\"");
        assert_eq!(wire(&VolumeBucket::UnderMillion), "\"<1M\"");
        assert_eq!(wire(&VolumeBucket::UnderTenMillion), "\"<10M\"");
        assert_eq!(wire(&VolumeBucket::UnderHundredMillion), "\"<100M\"");
        assert_eq!(wire(&VolumeBucket::HundredMillionOrMore), "\"100M+\"");

        assert_eq!(wire(&UptimeBucket::UnderHour), "\"<1h\"");
        assert_eq!(wire(&UptimeBucket::HourToDay), "\"1h-1d\"");
        assert_eq!(wire(&UptimeBucket::DayToWeek), "\"1d-7d\"");
        assert_eq!(wire(&UptimeBucket::WeekToMonth), "\"7d-30d\"");
        assert_eq!(wire(&UptimeBucket::MonthOrMore), "\"30d+\"");

        assert_eq!(wire(&InstallAgeBucket::UnderDay), "\"<1d\"");
        assert_eq!(wire(&InstallAgeBucket::DayToWeek), "\"1d-7d\"");
        assert_eq!(wire(&InstallAgeBucket::WeekToMonth), "\"7d-30d\"");
        assert_eq!(wire(&InstallAgeBucket::MonthToQuarter), "\"30d-90d\"");
        assert_eq!(wire(&InstallAgeBucket::QuarterToYear), "\"90d-1y\"");
        assert_eq!(wire(&InstallAgeBucket::YearOrMore), "\"1y+\"");
    }
}
