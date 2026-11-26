//! Types and schema module for `cetra-column`.

use std::fmt;
use std::sync::Arc;

use crate::ColumnError;

/// Precision of `Decimal128` used for deterministic cost sums (D-A2-4).
pub const COST_DECIMAL_PRECISION: u8 = 38;
/// Scale of `Decimal128` used for deterministic cost sums (D-A2-4).
pub const COST_DECIMAL_SCALE: i8 = 18;
/// Maximum allowed `Decimal128` precision.
pub const MAX_DECIMAL_PRECISION: u8 = 38;
/// Maximum allowed recursive `DataType::List` nesting depth (`Utf8` = 0, `List(Utf8)` = 1).
pub const MAX_NESTING_DEPTH: usize = 8;

/// Calendar-aware SQL interval (`months`, `days`, `nanos`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Interval {
    /// Whole calendar months (may be negative).
    pub months: i32,
    /// Whole calendar days (may be negative).
    pub days: i32,
    /// Nanoseconds within a day (may be negative).
    pub nanos: i64,
}

impl Interval {
    /// Canonical nanosecond key (`30` days per month, `86_400_000_000_000` ns per day)
    /// for Postgres/DataFusion-compatible interval comparison and hashing while preserving
    /// exact `(months, days, nanos)` fields for serialization and calendar arithmetic.
    pub const fn canonical_key(&self) -> i128 {
        self.months as i128 * 30 * 86_400_000_000_000
            + self.days as i128 * 86_400_000_000_000
            + self.nanos as i128
    }
}

/// Concrete data types supported by `cetra-column`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DataType {
    /// Untyped SQL `NULL`.
    Null,
    /// Boolean.
    Bool,
    /// Signed 64-bit integer.
    I64,
    /// IEEE-754 64-bit float.
    F64,
    /// Fixed-precision 128-bit decimal (`1 <= precision <= 38`, `-precision <= scale <= precision`).
    Decimal128 {
        /// Total decimal digits (`1..=38`).
        precision: u8,
        /// Fractional digits.
        scale: i8,
    },
    /// Nanoseconds since Unix epoch (`utc = true` for stored/zoned timestamps, `false` for naive).
    TimestampNs {
        /// Whether the instant is UTC-zoned.
        utc: bool,
    },
    /// Days since Unix epoch (1970-01-01).
    Date,
    /// Calendar interval (`months, days, nanos`).
    Interval,
    /// UTF-8 string.
    Utf8,
    /// Variable-length binary bytes.
    Binary,
    /// Fixed 8-byte binary (`SpanId`).
    FixedBytes8,
    /// Fixed 16-byte binary (`TraceId`).
    FixedBytes16,
    /// Homogeneous list of elements.
    List(Box<DataType>),
    /// Map of non-null UTF-8 keys to nullable UTF-8 values.
    MapUtf8,
    /// Typed list of span events.
    Events,
    /// Typed list of span links.
    Links,
}

impl DataType {
    /// `Decimal128(38, 18)` used for deterministic cost sums.
    pub const fn cost_decimal() -> Self {
        Self::Decimal128 {
            precision: COST_DECIMAL_PRECISION,
            scale: COST_DECIMAL_SCALE,
        }
    }

    /// UTC nanosecond timestamp (`TimestampNs { utc: true }`).
    pub const fn timestamp_utc() -> Self {
        Self::TimestampNs { utc: true }
    }

    /// Validates `Decimal128` precision/scale bounds and recursive `List` nesting depth.
    pub fn validate(&self, max_depth: usize) -> Result<(), ColumnError> {
        match self {
            Self::Decimal128 { precision, scale } => {
                if *precision == 0 || *precision > MAX_DECIMAL_PRECISION {
                    return Err(ColumnError::InvalidBatch {
                        reason: "decimal precision out of range (1..=38)",
                    });
                }
                if *scale < -(*precision as i8) || *scale > *precision as i8 {
                    return Err(ColumnError::InvalidBatch {
                        reason: "decimal scale out of valid range for precision",
                    });
                }
            }
            Self::List(inner) => {
                if max_depth == 0 {
                    return Err(ColumnError::InvalidBatch {
                        reason: "exceeded maximum nesting depth",
                    });
                }
                inner.validate(max_depth - 1)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Unified cetra `columns[].type` display name (D-QE-15).
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool => "boolean",
            Self::I64 => "integer",
            Self::F64 => "float",
            Self::Decimal128 { .. } => "decimal",
            Self::TimestampNs { .. } => "timestamp",
            Self::Date => "date",
            Self::Interval => "interval",
            Self::Utf8 => "string",
            Self::Binary | Self::FixedBytes8 | Self::FixedBytes16 => "binary",
            Self::List(inner) if matches!(inner.as_ref(), Self::Utf8) => "string_list",
            Self::List(_) => "list",
            Self::MapUtf8 => "string_map",
            Self::Events | Self::Links => "json",
        }
    }
}

/// One column in a [`Schema`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Field {
    /// Column name.
    pub name: String,
    /// Column data type.
    pub data_type: DataType,
    /// Whether rows in this column may be null.
    pub nullable: bool,
}

impl Field {
    /// Creates a new field.
    pub fn new(name: impl Into<String>, data_type: DataType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable,
        }
    }
}

/// Ordered list of fields describing a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    fields: Vec<Field>,
}

impl Schema {
    /// Builds a schema; rejects empty field names or invalid data types.
    pub fn try_new(fields: Vec<Field>) -> Result<Self, ColumnError> {
        for f in &fields {
            if f.name.is_empty() {
                return Err(ColumnError::InvalidBatch {
                    reason: "field name cannot be empty",
                });
            }
            f.data_type.validate(MAX_NESTING_DEPTH)?;
        }
        Ok(Self { fields })
    }

    /// Builds a schema wrapped in `Arc`.
    pub fn try_new_arc(fields: Vec<Field>) -> Result<Arc<Self>, ColumnError> {
        Ok(Arc::new(Self::try_new(fields)?))
    }

    /// Fields in column order.
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// Number of columns.
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// True when the schema has zero columns.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// First column index with exact name `name`.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }

    /// Projects columns by index.
    pub fn project(&self, indices: &[usize]) -> Result<Self, ColumnError> {
        let mut proj = Vec::with_capacity(indices.len());
        for &idx in indices {
            let field = self.fields.get(idx).ok_or(ColumnError::InvalidBatch {
                reason: "projection index out of bounds",
            })?;
            proj.push(field.clone());
        }
        Self::try_new(proj)
    }
}

/// One typed span event item.
#[derive(Clone, PartialEq, Eq)]
pub struct EventItem {
    /// Event timestamp in UTC nanoseconds.
    pub time: i64,
    /// Event name.
    pub name: String,
    /// Optional JSON attributes string.
    pub attributes: Option<String>,
}

impl fmt::Debug for EventItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventItem")
            .field("time", &self.time)
            .field("name_len", &self.name.len())
            .field("attributes_len", &self.attributes.as_ref().map(String::len))
            .finish()
    }
}

/// One typed span link item.
#[derive(Clone, PartialEq, Eq)]
pub struct LinkItem {
    /// Linked 16-byte trace id.
    pub trace_id: [u8; 16],
    /// Linked 8-byte span id.
    pub span_id: [u8; 8],
    /// Optional JSON attributes string.
    pub attributes: Option<String>,
}

impl fmt::Debug for LinkItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkItem")
            .field("attributes_len", &self.attributes.as_ref().map(String::len))
            .finish()
    }
}

/// Single scalar value used by `Column::Const` and literals.
#[derive(Clone, PartialEq)]
pub enum ScalarValue {
    /// Typed or untyped null.
    Null(DataType),
    /// Boolean.
    Bool(bool),
    /// Signed 64-bit integer.
    I64(i64),
    /// IEEE-754 64-bit float.
    F64(f64),
    /// Fixed-precision 128-bit decimal.
    Decimal128 {
        /// Unscaled integer value.
        value: i128,
        /// Precision (`1..=38`).
        precision: u8,
        /// Scale.
        scale: i8,
    },
    /// Nanosecond timestamp.
    TimestampNs {
        /// Nanoseconds since epoch.
        value: i64,
        /// True when UTC-zoned.
        utc: bool,
    },
    /// Days since Unix epoch.
    Date(i32),
    /// Calendar interval.
    Interval(Interval),
    /// UTF-8 string.
    Utf8(String),
    /// Variable-width binary.
    Binary(Vec<u8>),
    /// 8-byte fixed binary.
    FixedBytes8([u8; 8]),
    /// 16-byte fixed binary.
    FixedBytes16([u8; 16]),
    /// Homogeneous list scalar.
    List {
        /// Element type.
        item_type: DataType,
        /// List elements.
        values: Vec<ScalarValue>,
    },
    /// String-to-nullable-string map scalar.
    MapUtf8(Vec<(String, Option<String>)>),
    /// Events list scalar.
    Events(Vec<EventItem>),
    /// Links list scalar.
    Links(Vec<LinkItem>),
}

impl fmt::Debug for ScalarValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null(dt) => f.debug_tuple("Null").field(dt).finish(),
            Self::Bool(v) => f.debug_tuple("Bool").field(v).finish(),
            Self::I64(v) => f.debug_tuple("I64").field(v).finish(),
            Self::F64(v) => f.debug_tuple("F64").field(v).finish(),
            Self::Decimal128 {
                precision, scale, ..
            } => f
                .debug_struct("Decimal128")
                .field("precision", precision)
                .field("scale", scale)
                .finish(),
            Self::TimestampNs { utc, .. } => {
                f.debug_struct("TimestampNs").field("utc", utc).finish()
            }
            Self::Date(v) => f.debug_tuple("Date").field(v).finish(),
            Self::Interval(i) => f.debug_tuple("Interval").field(i).finish(),
            Self::Utf8(s) => f.debug_struct("Utf8").field("len", &s.len()).finish(),
            Self::Binary(b) => f.debug_struct("Binary").field("len", &b.len()).finish(),
            Self::FixedBytes8(_) => f.write_str("FixedBytes8"),
            Self::FixedBytes16(_) => f.write_str("FixedBytes16"),
            Self::List { item_type, values } => f
                .debug_struct("List")
                .field("item_type", item_type)
                .field("len", &values.len())
                .finish(),
            Self::MapUtf8(m) => f
                .debug_struct("MapUtf8")
                .field("entries", &m.len())
                .finish(),
            Self::Events(e) => f.debug_struct("Events").field("len", &e.len()).finish(),
            Self::Links(l) => f.debug_struct("Links").field("len", &l.len()).finish(),
        }
    }
}

impl ScalarValue {
    /// Concrete [`DataType`] of this scalar.
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Null(dt) => dt.clone(),
            Self::Bool(_) => DataType::Bool,
            Self::I64(_) => DataType::I64,
            Self::F64(_) => DataType::F64,
            Self::Decimal128 {
                precision, scale, ..
            } => DataType::Decimal128 {
                precision: *precision,
                scale: *scale,
            },
            Self::TimestampNs { utc, .. } => DataType::TimestampNs { utc: *utc },
            Self::Date(_) => DataType::Date,
            Self::Interval(_) => DataType::Interval,
            Self::Utf8(_) => DataType::Utf8,
            Self::Binary(_) => DataType::Binary,
            Self::FixedBytes8(_) => DataType::FixedBytes8,
            Self::FixedBytes16(_) => DataType::FixedBytes16,
            Self::List { item_type, .. } => DataType::List(Box::new(item_type.clone())),
            Self::MapUtf8(_) => DataType::MapUtf8,
            Self::Events(_) => DataType::Events,
            Self::Links(_) => DataType::Links,
        }
    }

    /// True if `self` is [`ScalarValue::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null(_))
    }

    /// Validates nested `DataType` bounds, `Decimal128` precision/scale, and `List` element types.
    pub fn validate(&self, max_depth: usize) -> Result<(), ColumnError> {
        match self {
            Self::Null(dt) => dt.validate(max_depth),
            Self::Decimal128 {
                precision, scale, ..
            } => {
                let dt = DataType::Decimal128 {
                    precision: *precision,
                    scale: *scale,
                };
                dt.validate(max_depth)
            }
            Self::List { item_type, values } => {
                if max_depth == 0 {
                    return Err(ColumnError::InvalidBatch {
                        reason: "exceeded maximum nesting depth",
                    });
                }
                item_type.validate(max_depth - 1)?;
                for v in values {
                    v.validate(max_depth - 1)?;
                    if v.data_type() != *item_type {
                        return Err(ColumnError::InvalidBatch {
                            reason: "list scalar element type does not match item_type",
                        });
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// NaN-aware semantic equality.
    pub fn semantic_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null(a), Self::Null(b)) => a == b,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::I64(a), Self::I64(b)) => a == b,
            (Self::F64(a), Self::F64(b)) => (a.is_nan() && b.is_nan()) || a == b,
            (
                Self::Decimal128 {
                    value: v1,
                    precision: p1,
                    scale: s1,
                },
                Self::Decimal128 {
                    value: v2,
                    precision: p2,
                    scale: s2,
                },
            ) => v1 == v2 && p1 == p2 && s1 == s2,
            (
                Self::TimestampNs { value: v1, utc: u1 },
                Self::TimestampNs { value: v2, utc: u2 },
            ) => v1 == v2 && u1 == u2,
            (Self::Date(a), Self::Date(b)) => a == b,
            (Self::Interval(a), Self::Interval(b)) => a == b,
            (Self::Utf8(a), Self::Utf8(b)) => a == b,
            (Self::Binary(a), Self::Binary(b)) => a == b,
            (Self::FixedBytes8(a), Self::FixedBytes8(b)) => a == b,
            (Self::FixedBytes16(a), Self::FixedBytes16(b)) => a == b,
            (
                Self::List {
                    item_type: it1,
                    values: v1,
                },
                Self::List {
                    item_type: it2,
                    values: v2,
                },
            ) => {
                it1 == it2
                    && v1.len() == v2.len()
                    && v1.iter().zip(v2).all(|(a, b)| a.semantic_eq(b))
            }
            (Self::MapUtf8(a), Self::MapUtf8(b)) => a == b,
            (Self::Events(a), Self::Events(b)) => a == b,
            (Self::Links(a), Self::Links(b)) => a == b,
            _ => false,
        }
    }

    /// Deterministic owned logical heap bytes.
    pub fn heap_bytes(&self) -> usize {
        match self {
            Self::Utf8(s) => s.len(),
            Self::Binary(b) => b.len(),
            Self::List { values, .. } => {
                values.len() * size_of::<ScalarValue>()
                    + values.iter().map(|v| v.heap_bytes()).sum::<usize>()
            }
            Self::MapUtf8(m) => {
                m.len() * size_of::<(String, Option<String>)>()
                    + m.iter()
                        .map(|(k, v)| k.len() + v.as_ref().map_or(0, String::len))
                        .sum::<usize>()
            }
            Self::Events(e) => {
                e.len() * size_of::<EventItem>()
                    + e.iter()
                        .map(|ev| ev.name.len() + ev.attributes.as_ref().map_or(0, String::len))
                        .sum::<usize>()
            }
            Self::Links(l) => {
                l.len() * size_of::<LinkItem>()
                    + l.iter()
                        .map(|lk| lk.attributes.as_ref().map_or(0, String::len))
                        .sum::<usize>()
            }
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_match_d_qe_15() {
        assert_eq!(DataType::Null.display_name(), "null");
        assert_eq!(DataType::Bool.display_name(), "boolean");
        assert_eq!(DataType::I64.display_name(), "integer");
        assert_eq!(DataType::F64.display_name(), "float");
        assert_eq!(
            DataType::Decimal128 {
                precision: 10,
                scale: 2
            }
            .display_name(),
            "decimal"
        );
        assert_eq!(
            DataType::TimestampNs { utc: true }.display_name(),
            "timestamp"
        );
        assert_eq!(DataType::Date.display_name(), "date");
        assert_eq!(DataType::Interval.display_name(), "interval");
        assert_eq!(DataType::Utf8.display_name(), "string");
        assert_eq!(DataType::Binary.display_name(), "binary");
        assert_eq!(DataType::FixedBytes8.display_name(), "binary");
        assert_eq!(DataType::FixedBytes16.display_name(), "binary");
        assert_eq!(
            DataType::List(Box::new(DataType::Utf8)).display_name(),
            "string_list"
        );
        assert_eq!(
            DataType::List(Box::new(DataType::I64)).display_name(),
            "list"
        );
        assert_eq!(DataType::MapUtf8.display_name(), "string_map");
        assert_eq!(DataType::Events.display_name(), "json");
        assert_eq!(DataType::Links.display_name(), "json");
    }

    #[test]
    fn decimal_validation_rejects_bad_precision_and_scale() {
        assert!(
            DataType::Decimal128 {
                precision: 0,
                scale: 0
            }
            .validate(8)
            .is_err()
        );
        assert!(
            DataType::Decimal128 {
                precision: 39,
                scale: 0
            }
            .validate(8)
            .is_err()
        );
        assert!(
            DataType::Decimal128 {
                precision: 10,
                scale: 11
            }
            .validate(8)
            .is_err()
        );
        assert!(
            DataType::Decimal128 {
                precision: 10,
                scale: -11
            }
            .validate(8)
            .is_err()
        );
        assert!(
            DataType::Decimal128 {
                precision: 10,
                scale: 2
            }
            .validate(8)
            .is_ok()
        );
    }

    #[test]
    fn list_depth_validation_rejects_excessive_nesting() {
        let mut dt = DataType::Utf8;
        for _ in 0..8 {
            dt = DataType::List(Box::new(dt));
        }
        assert!(dt.validate(8).is_ok());

        let mut dt2 = DataType::Utf8;
        for _ in 0..9 {
            dt2 = DataType::List(Box::new(dt2));
        }
        assert!(dt2.validate(8).is_err());
    }

    #[test]
    fn scalar_debug_redacts_payloads_and_semantic_eq_handles_nan() {
        let cases = [
            ScalarValue::Utf8("secret_payload_marker".into()),
            ScalarValue::Binary(b"secret_payload_marker".to_vec()),
            ScalarValue::FixedBytes8(*b"secret8!"),
            ScalarValue::FixedBytes16(*b"secret16_marker!"),
            ScalarValue::MapUtf8(vec![(
                "secret_key".into(),
                Some("secret_payload_marker".into()),
            )]),
            ScalarValue::Events(vec![EventItem {
                time: 42,
                name: "secret_event".into(),
                attributes: Some("secret_payload_marker".into()),
            }]),
            ScalarValue::Links(vec![LinkItem {
                trace_id: *b"secret16_marker!",
                span_id: *b"secret8!",
                attributes: Some("secret_payload_marker".into()),
            }]),
        ];
        for scalar in &cases {
            let dbg = format!("{scalar:?}");
            assert!(!dbg.contains("secret"), "leaked in {dbg}");
        }

        let nan1 = ScalarValue::F64(f64::NAN);
        let nan2 = ScalarValue::F64(f64::NAN);
        assert!(nan1.semantic_eq(&nan2));

        let bad_list = ScalarValue::List {
            item_type: DataType::I64,
            values: vec![ScalarValue::Utf8("wrong".into())],
        };
        assert!(bad_list.validate(MAX_NESTING_DEPTH).is_err());
    }

    #[test]
    fn interval_canonical_key_matches_equivalent_durations() {
        let one_day = Interval {
            months: 0,
            days: 1,
            nanos: 0,
        };
        let day_nanos = Interval {
            months: 0,
            days: 0,
            nanos: 86_400_000_000_000,
        };
        assert_eq!(one_day.canonical_key(), day_nanos.canonical_key());

        let one_month = Interval {
            months: 1,
            days: 0,
            nanos: 0,
        };
        let thirty_days = Interval {
            months: 0,
            days: 30,
            nanos: 0,
        };
        assert_eq!(one_month.canonical_key(), thirty_days.canonical_key());
    }
}
