// SPDX-License-Identifier: Elastic-2.0
//! RFC 3339 instants in claims, as nanoseconds since the Unix epoch. Deliberately as lax as
//! chrono's parser: tokens are produced by our own signers, and offsets are converted to UTC.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serializer};

/// Writes `ns` as RFC 3339 UTC with nanoseconds and a `Z` suffix.
pub(crate) fn serialize<S: Serializer>(ns: &i64, serializer: S) -> Result<S::Ok, S::Error> {
    let instant = DateTime::<Utc>::from_timestamp_nanos(*ns);
    serializer.serialize_str(&instant.to_rfc3339_opts(SecondsFormat::Nanos, true))
}

/// Reads an RFC 3339 instant. The error never contains the input.
pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
    let text = String::deserialize(deserializer)?;
    let parsed = DateTime::parse_from_rfc3339(&text)
        .map_err(|_| D::Error::custom("invalid RFC 3339 instant"))?;
    parsed
        .with_timezone(&Utc)
        .timestamp_nanos_opt()
        .ok_or_else(|| D::Error::custom("invalid RFC 3339 instant"))
}
