//! Predicates over the narrow columns of a file: the public predicate types, their binding to
//! a file (type checks and literal conversion), their evaluation with Arrow's kernels under SQL
//! three-valued logic, and the zone and dictionary rules that prune chunks (design S7 § scan).

use std::collections::BTreeSet;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, AsArray, BinaryArray, BooleanArray, DictionaryArray, FixedSizeBinaryArray,
    Float64Array, Int64Array, MapArray, Scalar, StringArray, UInt32Array, UInt64Array,
    new_null_array,
};
use arrow::buffer::BooleanBuffer;
use arrow::compute::kernels::cmp::{eq, gt, gt_eq, lt, lt_eq, neq};
use arrow::compute::{
    CastOptions, and_kleene, cast, cast_with_options, is_not_null, is_null, not, or_kleene,
};
use arrow::datatypes::{DataType, Field, UInt32Type};

use crate::error::{Result, StrataError};
use crate::file::StrataFile;
use crate::hot::Layer;
use crate::narrow::{
    BlockEntry, DictKind, Dictionary, Encoding, SHARED_SUFFIX, flags, prefix_hash,
};
use crate::payload::MAX_NEEDLE;

/// A literal in a predicate.
#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
    /// SQL NULL.
    Null,
    /// A boolean.
    Bool(bool),
    /// A signed integer; also a timestamp in the column's unit.
    Int(i64),
    /// An unsigned integer.
    UInt(u64),
    /// A floating-point number.
    Float(f64),
    /// A string.
    Utf8(String),
    /// Bytes, for Binary and FixedSizeBinary columns.
    Bytes(Vec<u8>),
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// Equal.
    Eq,
    /// Not equal.
    NotEq,
    /// Less than.
    Lt,
    /// Less than or equal.
    LtEq,
    /// Greater than.
    Gt,
    /// Greater than or equal.
    GtEq,
}

/// A boolean predicate over the columns of one file. Columns are schema indexes.
///
/// Comparisons, `In`, `IsNull` and `IsNotNull` take part in SQL three-valued logic, and may name
/// only narrow columns. `MapKeyEq` may name only a shredded map column, and is NULL unless the
/// key has exactly one non-null value in the row's map. `Contains` may name only a payload
/// column; it is never NULL, so a null row is not a match and `Not(Contains(..))` selects it.
/// `Const` is TRUE, FALSE or NULL for every row.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    /// All of the terms; TRUE when empty.
    And(Vec<Predicate>),
    /// Any of the terms; FALSE when empty.
    Or(Vec<Predicate>),
    /// The negation.
    Not(Box<Predicate>),
    /// `column op literal`.
    Cmp(usize, CmpOp, Lit),
    /// `column IN (literals)`.
    In(usize, Vec<Lit>),
    /// `column IS NULL`.
    IsNull(usize),
    /// `column IS NOT NULL`.
    IsNotNull(usize),
    /// `map_column['key'] = literal`: the single non-null value of `key` in the row's map,
    /// compared with the literal. NULL when the map is null, the key is absent, its value is
    /// null, or the key occurs more than once. The literal must be `Lit::Utf8` or `Lit::Null`.
    MapKeyEq(usize, String, Lit),
    /// The payload column's value contains the bytes. Never NULL: a null row is FALSE. The
    /// needle is at most 64 bytes long.
    Contains(usize, Vec<u8>),
    /// A constant: TRUE (`Some(true)`), FALSE (`Some(false)`) or NULL (`None`) for every row.
    Const(Option<bool>),
}

/// A predicate checked against a file, with its literals converted to the column types.
#[derive(Debug, Clone)]
pub(crate) enum Bound {
    And(Vec<Bound>),
    Or(Vec<Bound>),
    Not(Box<Bound>),
    /// Directory id, operator, literal as a one-element array of the column type (or a
    /// one-element null array for `Lit::Null`).
    Cmp(usize, CmpOp, ArrayRef),
    IsNull(usize),
    IsNotNull(usize),
    /// A constant, as [`Predicate::Const`].
    Const(Option<bool>),
    /// The precomputed mask at this index of the masks of [`bind`].
    Mask(usize),
}

/// Where the mask of a [`Bound::Mask`] leaf comes from.
#[derive(Debug, Clone)]
pub(crate) enum MaskSource {
    /// `Contains(column, needle)` on a payload column. The scan computes the whole column's mask.
    Contains(usize, Vec<u8>),
    /// A `MapKeyEq` on a key that is not promoted: the residual column of the map, the key and
    /// the literal, as a one-element Utf8 array (or a null one). Computed per row range.
    MapKey(usize, String, ArrayRef),
}

/// The field of column `column`, which must be a narrow column of the file.
fn narrow_field(file: &StrataFile, column: usize) -> Result<Field> {
    let schema = file.schema();
    let field = schema
        .fields()
        .get(column)
        .ok_or_else(|| StrataError::Schema {
            reason: format!("no column {column}"),
        })?;
    let record = file
        .directory()
        .get(column)
        .ok_or_else(|| StrataError::Schema {
            reason: format!("no column {column}"),
        })?;
    if record.layer != Layer::Narrow {
        return Err(StrataError::Unsupported {
            what: format!("predicate on column {column}"),
        });
    }
    Ok(field.as_ref().clone())
}

/// Whether `Cmp` and `In` may compare a column of this type.
fn is_comparable(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Boolean
            | DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
            | DataType::Float32
            | DataType::Float64
            | DataType::Timestamp(_, _)
            | DataType::Date32
            | DataType::Date64
            | DataType::Utf8
            | DataType::Binary
            | DataType::FixedSizeBinary(_)
    )
}

/// The field of narrow column `column`, which must also be comparable.
fn comparable_field(file: &StrataFile, column: usize) -> Result<Field> {
    let field = narrow_field(file, column)?;
    if is_comparable(field.data_type()) {
        Ok(field)
    } else {
        Err(StrataError::Unsupported {
            what: format!("predicate on column {column}"),
        })
    }
}

/// Whether a literal of an integer kind may be converted to `dt` (the table of M4.2's binding).
fn accepts_integers(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
            | DataType::Timestamp(_, _)
            | DataType::Date32
            | DataType::Date64
            | DataType::Float32
            | DataType::Float64
    )
}

/// Casts `array` to `dt` without overflow. A failed cast is an unsupported literal of `column`.
fn cast_literal(array: ArrayRef, dt: &DataType, column: usize) -> Result<ArrayRef> {
    let options = CastOptions {
        safe: false,
        ..Default::default()
    };
    cast_with_options(&array, dt, &options).map_err(|_| StrataError::Unsupported {
        what: format!("literal type for column {column}"),
    })
}

/// Converts `lit` to a one-element array of `dt`, the type of `column`.
pub(crate) fn literal(column: usize, dt: &DataType, lit: &Lit) -> Result<ArrayRef> {
    let unsupported = || StrataError::Unsupported {
        what: format!("literal type for column {column}"),
    };
    match (lit, dt) {
        (Lit::Null, _) => Ok(new_null_array(dt, 1)),
        (Lit::Bool(b), DataType::Boolean) => Ok(Arc::new(BooleanArray::from(vec![*b]))),
        (Lit::Int(v), _) if accepts_integers(dt) => {
            cast_literal(Arc::new(Int64Array::from(vec![*v])), dt, column)
        }
        (Lit::UInt(v), _) if accepts_integers(dt) => {
            cast_literal(Arc::new(UInt64Array::from(vec![*v])), dt, column)
        }
        (Lit::Float(v), DataType::Float32 | DataType::Float64) => {
            cast_literal(Arc::new(Float64Array::from(vec![*v])), dt, column)
        }
        (Lit::Utf8(s), DataType::Utf8) => Ok(Arc::new(StringArray::from(vec![s.as_str()]))),
        (Lit::Bytes(b), DataType::Binary) => Ok(Arc::new(BinaryArray::from(vec![b.as_slice()]))),
        (Lit::Bytes(b), DataType::FixedSizeBinary(width))
            if usize::try_from(*width).ok() == Some(b.len()) =>
        {
            let array = FixedSizeBinaryArray::try_from_iter(std::iter::once(b.as_slice()))
                .map_err(|_| unsupported())?;
            Ok(Arc::new(array))
        }
        _ => Err(unsupported()),
    }
}

/// Binds `p` to `file`: checks every column and literal, and converts the literals once. It
/// returns the bound predicate and the sources of its masks, which the [`Bound::Mask`] leaves
/// index.
///
/// Errors are reported before any read, so they never depend on pruning. A `MapKeyEq` on a
/// promoted key binds to the key's child column; on any other key it becomes a mask.
pub(crate) fn bind(p: &Predicate, file: &StrataFile) -> Result<(Bound, Vec<MaskSource>)> {
    check_size(p)?;
    let mut masks = Vec::new();
    let bound = bind_into(p, file, &mut masks)?;
    Ok((bound, masks))
}

/// The deepest predicate `bind` accepts. Deeper predicates are rejected before any recursion.
pub(crate) const MAX_PREDICATE_DEPTH: usize = 64;

/// The largest number of nodes `bind` accepts. Each node counts once, and each literal of an
/// `In` counts once more.
pub(crate) const MAX_PREDICATE_NODES: usize = 4_096;

/// Rejects a predicate deeper than [`MAX_PREDICATE_DEPTH`] or larger than
/// [`MAX_PREDICATE_NODES`]. It walks the predicate with an explicit stack, so the check itself
/// never recurses.
pub(crate) fn check_size(p: &Predicate) -> Result<()> {
    let too_large = || StrataError::Unsupported {
        what: "predicate too large".to_string(),
    };
    let mut stack: Vec<(&Predicate, usize)> = vec![(p, 1)];
    let mut nodes = 0usize;
    while let Some((node, depth)) = stack.pop() {
        if depth > MAX_PREDICATE_DEPTH {
            return Err(too_large());
        }
        nodes = nodes.checked_add(1).ok_or_else(too_large)?;
        match node {
            Predicate::And(terms) | Predicate::Or(terms) => {
                for term in terms {
                    stack.push((term, depth + 1));
                }
            }
            Predicate::Not(inner) => stack.push((inner.as_ref(), depth + 1)),
            Predicate::In(_, lits) => {
                nodes = nodes.checked_add(lits.len()).ok_or_else(too_large)?;
            }
            Predicate::Cmp(..)
            | Predicate::IsNull(_)
            | Predicate::IsNotNull(_)
            | Predicate::MapKeyEq(..)
            | Predicate::Contains(..)
            | Predicate::Const(_) => {}
        }
        if nodes > MAX_PREDICATE_NODES {
            return Err(too_large());
        }
    }
    Ok(())
}

/// The layer of schema column `column`.
fn schema_layer(file: &StrataFile, column: usize) -> Result<Layer> {
    let no_column = || StrataError::Schema {
        reason: format!("no column {column}"),
    };
    if column >= file.schema().fields().len() {
        return Err(no_column());
    }
    file.directory()
        .get(column)
        .map(|record| record.layer)
        .ok_or_else(no_column)
}

/// The shredding layout of map column `column`.
pub(crate) fn layout_of(file: &StrataFile, column: usize) -> Result<&crate::hot::MapLayout> {
    file.hot
        .maps
        .iter()
        .find(|layout| usize::try_from(layout.map).ok() == Some(column))
        .ok_or_else(|| StrataError::Corrupt {
            what: "map layout",
            offset: 0,
        })
}

/// Binds one predicate, appending the sources of its masks to `masks`.
fn bind_into(p: &Predicate, file: &StrataFile, masks: &mut Vec<MaskSource>) -> Result<Bound> {
    match p {
        Predicate::And(terms) => {
            let mut bound = Vec::with_capacity(terms.len());
            for term in terms {
                bound.push(bind_into(term, file, masks)?);
            }
            Ok(Bound::And(bound))
        }
        Predicate::Or(terms) => {
            let mut bound = Vec::with_capacity(terms.len());
            for term in terms {
                bound.push(bind_into(term, file, masks)?);
            }
            Ok(Bound::Or(bound))
        }
        Predicate::Not(inner) => Ok(Bound::Not(Box::new(bind_into(inner, file, masks)?))),
        Predicate::Cmp(column, op, lit) => {
            let field = comparable_field(file, *column)?;
            let array = literal(*column, field.data_type(), lit)?;
            Ok(Bound::Cmp(*column, *op, array))
        }
        Predicate::In(column, lits) => {
            let field = comparable_field(file, *column)?;
            let mut terms = Vec::with_capacity(lits.len());
            for lit in lits {
                let array = literal(*column, field.data_type(), lit)?;
                terms.push(Bound::Cmp(*column, CmpOp::Eq, array));
            }
            Ok(Bound::Or(terms))
        }
        Predicate::IsNull(column) => {
            narrow_field(file, *column)?;
            Ok(Bound::IsNull(*column))
        }
        Predicate::IsNotNull(column) => {
            narrow_field(file, *column)?;
            Ok(Bound::IsNotNull(*column))
        }
        Predicate::MapKeyEq(column, key, lit) => {
            if schema_layer(file, *column)? != Layer::MapParent {
                return Err(StrataError::Unsupported {
                    what: format!("map key predicate on column {column}"),
                });
            }
            // Only Utf8 or null literals convert to the Utf8 values of a map.
            let value = literal(*column, &DataType::Utf8, lit)?;
            let layout = layout_of(file, *column)?;
            let corrupt = || StrataError::Corrupt {
                what: "map layout",
                offset: 0,
            };
            if let Some(entry) = layout
                .keys
                .iter()
                .find(|entry| entry.key.as_slice() == key.as_bytes())
            {
                let child = usize::try_from(entry.child).map_err(|_| corrupt())?;
                return Ok(Bound::Cmp(child, CmpOp::Eq, value));
            }
            let residual = usize::try_from(layout.residual).map_err(|_| corrupt())?;
            masks.push(MaskSource::MapKey(residual, key.clone(), value));
            Ok(Bound::Mask(masks.len() - 1))
        }
        Predicate::Const(value) => Ok(Bound::Const(*value)),
        Predicate::Contains(column, needle) => {
            if schema_layer(file, *column)? != Layer::Payload {
                return Err(StrataError::Unsupported {
                    what: format!("contains on column {column}"),
                });
            }
            if needle.len() > MAX_NEEDLE {
                return Err(StrataError::Unsupported {
                    what: format!("needle of {} bytes on column {column}", needle.len()),
                });
            }
            masks.push(MaskSource::Contains(*column, needle.clone()));
            Ok(Bound::Mask(masks.len() - 1))
        }
    }
}

/// The column `column` of `columns`, which holds the decoded rows of every column a predicate
/// reads.
fn column_of(columns: &[Option<ArrayRef>], column: usize) -> Result<&ArrayRef> {
    columns
        .get(column)
        .and_then(Option::as_ref)
        .ok_or_else(|| StrataError::Schema {
            reason: format!("internal: missing column {column}"),
        })
}

/// Evaluates `b` over `rows` rows. `columns` is indexed by directory id and holds the decoded
/// rows of every column `b` references; `masks` holds the masks of the [`Bound::Mask`] leaves,
/// in the order of [`bind`]. A row is selected only where the mask is valid and true: a null is
/// not a match.
/// A mask of `rows` rows, all `value`, without nulls: a bitmap, not a vector of booleans.
fn constant_mask(value: bool, rows: usize) -> BooleanArray {
    let bits = if value {
        BooleanBuffer::new_set(rows)
    } else {
        BooleanBuffer::new_unset(rows)
    };
    BooleanArray::new(bits, None)
}

pub(crate) fn evaluate(
    b: &Bound,
    columns: &[Option<ArrayRef>],
    masks: &[BooleanArray],
    rows: usize,
) -> Result<BooleanArray> {
    match b {
        // The first term's mask starts the fold; no term is TRUE for `And` and FALSE for `Or`.
        Bound::And(terms) => {
            let Some((first, rest)) = terms.split_first() else {
                return Ok(constant_mask(true, rows));
            };
            let mut acc = evaluate(first, columns, masks, rows)?;
            for term in rest {
                let mask = evaluate(term, columns, masks, rows)?;
                acc = and_kleene(&acc, &mask)?;
            }
            Ok(acc)
        }
        Bound::Or(terms) => {
            let Some((first, rest)) = terms.split_first() else {
                return Ok(constant_mask(false, rows));
            };
            let mut acc = evaluate(first, columns, masks, rows)?;
            for term in rest {
                let mask = evaluate(term, columns, masks, rows)?;
                acc = or_kleene(&acc, &mask)?;
            }
            Ok(acc)
        }
        Bound::Not(inner) => Ok(not(&evaluate(inner, columns, masks, rows)?)?),
        Bound::Mask(index) => masks
            .get(*index)
            .cloned()
            .ok_or_else(|| StrataError::Schema {
                reason: format!("internal: missing mask {index}"),
            }),
        Bound::Cmp(column, op, lit) => {
            let values = column_of(columns, *column)?;
            let array: &dyn Array = values.as_ref();
            if let Some(mask) = compare_codes(array, *op, lit.as_ref())? {
                return Ok(mask);
            }
            // A scan decodes a Utf8 column with a dictionary as a dictionary array; the
            // comparison kernels compare its values to a literal of the value type.
            let value_type = match array.data_type() {
                DataType::Dictionary(_, value) => value.as_ref(),
                other => other,
            };
            let lit = if lit.data_type() == value_type {
                lit.clone()
            } else {
                arrow::compute::cast(lit, value_type)?
            };
            let scalar = Scalar::new(lit);
            let mask = match op {
                CmpOp::Eq => eq(&array, &scalar)?,
                CmpOp::NotEq => neq(&array, &scalar)?,
                CmpOp::Lt => lt(&array, &scalar)?,
                CmpOp::LtEq => lt_eq(&array, &scalar)?,
                CmpOp::Gt => gt(&array, &scalar)?,
                CmpOp::GtEq => gt_eq(&array, &scalar)?,
            };
            Ok(mask)
        }
        Bound::IsNull(column) => {
            let array: &dyn Array = column_of(columns, *column)?.as_ref();
            Ok(is_null(array)?)
        }
        Bound::IsNotNull(column) => {
            let array: &dyn Array = column_of(columns, *column)?.as_ref();
            Ok(is_not_null(array)?)
        }
        Bound::Const(value) => Ok(BooleanArray::from(vec![*value; rows])),
    }
}

/// `array op lit` on the codes of a dictionary array, or `None` when the fast path does not
/// apply. A scan's dictionary arrays hold the file dictionary's values, which are sorted
/// bytewise (the order of Utf8 comparisons), so the literal's place in them turns every
/// comparison into one on the codes.
fn compare_codes(array: &dyn Array, op: CmpOp, lit: &dyn Array) -> Result<Option<BooleanArray>> {
    let Some(dict) = array.as_any().downcast_ref::<DictionaryArray<UInt32Type>>() else {
        return Ok(None);
    };
    let (Some(values), Some(lit)) = (
        dict.values().as_any().downcast_ref::<StringArray>(),
        lit.as_any().downcast_ref::<StringArray>(),
    ) else {
        return Ok(None);
    };
    if lit.len() != 1 || lit.is_null(0) || values.null_count() > 0 {
        return Ok(None);
    }
    let needle = lit.value(0).as_bytes();
    let point = |inclusive: bool| -> u32 {
        let (mut lo, mut hi) = (0usize, values.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let v = values.value(mid).as_bytes();
            if v < needle || (inclusive && v == needle) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        u32::try_from(lo).unwrap_or(u32::MAX)
    };
    let (lower, upper) = (point(false), point(true));
    let keys = dict.keys();
    let at = |code: u32| Scalar::new(UInt32Array::from(vec![code]));
    // Codes are below the dictionary's length, so `u32::MAX` matches no row.
    let mask = match op {
        CmpOp::Eq if lower < upper => eq(keys, &at(lower))?,
        CmpOp::Eq => eq(keys, &at(u32::MAX))?,
        CmpOp::NotEq if lower < upper => neq(keys, &at(lower))?,
        CmpOp::NotEq => neq(keys, &at(u32::MAX))?,
        CmpOp::Lt => lt(keys, &at(lower))?,
        CmpOp::LtEq => lt(keys, &at(upper))?,
        CmpOp::Gt => gt_eq(keys, &at(upper))?,
        CmpOp::GtEq => gt_eq(keys, &at(lower))?,
    };
    Ok(Some(mask))
}

/// The mask of `MapKeyEq(.., key, lit)` over the rows of a map array `map`, which is a map
/// column or the residual of a shredded one. A row is NULL when its map is null, when `lit` is
/// null, or when `key` does not occur exactly once in it with a non-null value. Otherwise the
/// row is `value == lit`.
pub(crate) fn map_key_mask(map: &dyn Array, key: &str, lit: &dyn Array) -> Result<BooleanArray> {
    let corrupt = || StrataError::Schema {
        reason: "internal: map of a key predicate".to_string(),
    };
    let map = map
        .as_any()
        .downcast_ref::<MapArray>()
        .ok_or_else(corrupt)?;
    let entries = map.entries();
    let keys = entries
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(corrupt)?;
    let values = entries
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(corrupt)?;
    let lit = lit
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(corrupt)?;
    let literal: Option<&str> = if lit.len() == 1 && lit.is_valid(0) {
        Some(lit.value(0))
    } else {
        None
    };
    let offsets = map.value_offsets();
    let bound = |row: usize| -> Result<usize> {
        offsets
            .get(row)
            .and_then(|offset| usize::try_from(*offset).ok())
            .ok_or_else(corrupt)
    };
    let mut out: Vec<Option<bool>> = Vec::with_capacity(map.len());
    for row in 0..map.len() {
        let Some(literal) = literal else {
            out.push(None);
            continue;
        };
        if map.is_null(row) {
            out.push(None);
            continue;
        }
        let (start, end) = (bound(row)?, bound(row + 1)?);
        let mut count = 0usize;
        let mut found = 0usize;
        for i in start..end {
            if keys.is_valid(i) && keys.value(i) == key {
                count += 1;
                found = i;
            }
        }
        let value = match count {
            1 if values.is_valid(found) => Some(values.value(found) == literal),
            _ => None,
        };
        out.push(value);
    }
    Ok(BooleanArray::from(out))
}

/// The conjuncts of `b`: the terms of `b` when it is an `And`, flattened through nested `And`s,
/// and `b` itself otherwise.
fn conjuncts(b: &Bound) -> Vec<&Bound> {
    fn walk<'a>(b: &'a Bound, out: &mut Vec<&'a Bound>) {
        match b {
            Bound::And(terms) => {
                for term in terms {
                    walk(term, out);
                }
            }
            other => out.push(other),
        }
    }
    let mut out = Vec::new();
    walk(b, &mut out);
    out
}

/// The column that conjunct `conjunct` prunes, if any. A conjunct prunes column `c` when it is
/// a comparison of `c` other than `!=`, or an `Or` whose terms are all equalities of `c` (an
/// `In`). An empty `Or` prunes nothing.
fn pruned_by(conjunct: &Bound) -> Option<usize> {
    match conjunct {
        Bound::Cmp(column, op, _) if *op != CmpOp::NotEq => Some(*column),
        Bound::Or(terms) => {
            let Some(Bound::Cmp(column, CmpOp::Eq, _)) = terms.first() else {
                return None;
            };
            let all_same = terms
                .iter()
                .all(|term| matches!(term, Bound::Cmp(c, CmpOp::Eq, _) if c == column));
            all_same.then_some(*column)
        }
        _ => None,
    }
}

/// The conjuncts of `b` that prune column `column`.
fn pruning_terms(b: &Bound, column: usize) -> Vec<&Bound> {
    conjuncts(b)
        .into_iter()
        .filter(|conjunct| pruned_by(conjunct) == Some(column))
        .collect()
}

/// Whether a top-level conjunct of `b` is FALSE or NULL. Such a conjunct selects no row, so a
/// scan can skip every chunk.
pub(crate) fn never_true(b: &Bound) -> bool {
    conjuncts(b)
        .into_iter()
        .any(|conjunct| matches!(conjunct, Bound::Const(Some(false) | None)))
}

/// The distinct columns that some conjunct of `b` prunes, in ascending order.
pub(crate) fn pruned_columns(b: &Bound) -> Vec<usize> {
    let columns: BTreeSet<usize> = conjuncts(b).into_iter().filter_map(pruned_by).collect();
    columns.into_iter().collect()
}

/// The distinct columns `b` reads, in ascending order.
pub(crate) fn bound_columns(b: &Bound) -> Vec<usize> {
    fn walk(b: &Bound, out: &mut BTreeSet<usize>) {
        match b {
            Bound::And(terms) | Bound::Or(terms) => {
                for term in terms {
                    walk(term, out);
                }
            }
            Bound::Not(inner) => walk(inner, out),
            Bound::Cmp(column, _, _) | Bound::IsNull(column) | Bound::IsNotNull(column) => {
                out.insert(*column);
            }
            Bound::Mask(_) | Bound::Const(_) => {}
        }
    }
    let mut columns = BTreeSet::new();
    walk(b, &mut columns);
    columns.into_iter().collect()
}

/// For a dictionary-encoded column `column`, the codes whose value satisfies every pruning
/// conjunct on `column`. `None` when there is no such conjunct or the column has no dictionary.
pub(crate) fn matching_codes(
    b: &Bound,
    column: usize,
    dict: Option<&Dictionary>,
) -> Result<Option<BTreeSet<u32>>> {
    // Only a Utf8 dictionary's codes prune: `chunk_may_match` reads them from `Dict` zones.
    let Some(dict) = dict.filter(|dict| dict.kind() == DictKind::Utf8) else {
        return Ok(None);
    };
    let terms = pruning_terms(b, column);
    if terms.is_empty() {
        return Ok(None);
    }
    let corrupt = || StrataError::Corrupt {
        what: "dictionary code",
        offset: 0,
    };
    let strings: ArrayRef = dict.values_array();
    let n = strings.len();
    let mut columns: Vec<Option<ArrayRef>> = vec![None; column + 1];
    if let Some(slot) = columns.get_mut(column) {
        *slot = Some(strings);
    }
    let mut keep = BooleanArray::from(vec![true; n]);
    for term in terms {
        let mask = evaluate(term, &columns, &[], n)?;
        keep = and_kleene(&keep, &mask)?;
    }
    let mut codes = BTreeSet::new();
    for i in 0..n {
        if keep.is_valid(i) && keep.value(i) {
            codes.insert(u32::try_from(i).map_err(|_| corrupt())?);
        }
    }
    Ok(Some(codes))
}

/// The one-element array of a literal read back as `i64`. `None` when it is null, or when the
/// cast fails, which means no pruning.
fn read_back_i64(lit: &ArrayRef) -> Option<i64> {
    let cast = cast(lit.as_ref(), &DataType::Int64).ok()?;
    let ints = cast.as_any().downcast_ref::<Int64Array>()?;
    ints.is_valid(0).then(|| ints.value(0))
}

/// The one-element array of a literal read back as `f64`. `None` when it is null, or when the
/// cast fails, which means no pruning.
fn read_back_f64(lit: &ArrayRef) -> Option<f64> {
    let cast = cast(lit.as_ref(), &DataType::Float64).ok()?;
    let floats = cast.as_any().downcast_ref::<Float64Array>()?;
    floats.is_valid(0).then(|| floats.value(0))
}

/// The first eight bytes of a zone bound, as a little-endian integer, or `None` when there are
/// fewer than eight bytes (which the 16-byte zone never has).
pub(crate) fn zone_i64(bound: &[u8; 16]) -> Option<i64> {
    bound
        .split_first_chunk::<8>()
        .map(|(head, _)| i64::from_le_bytes(*head))
}

/// The first eight bytes of a zone bound, as a little-endian `f64`.
fn zone_f64(bound: &[u8; 16]) -> Option<f64> {
    bound
        .split_first_chunk::<8>()
        .map(|(head, _)| f64::from_le_bytes(*head))
}

/// The first four bytes of a zone bound, as a little-endian `u32`: a dictionary code.
fn zone_u32(bound: &[u8; 16]) -> Option<u32> {
    bound
        .split_first_chunk::<4>()
        .map(|(head, _)| u32::from_le_bytes(*head))
}

/// Whether a range `[min, max]` may hold a value `x` with `x op v` TRUE. The chunk may match
/// unless the zone proves that no value in it can satisfy the comparison.
fn may_hold<T: PartialOrd + ?Sized>(op: CmpOp, min: &T, max: &T, v: &T) -> bool {
    match op {
        CmpOp::Eq => min <= v && v <= max,
        CmpOp::NotEq => true,
        CmpOp::Lt => min < v,
        CmpOp::LtEq => min <= v,
        CmpOp::Gt => max > v,
        CmpOp::GtEq => max >= v,
    }
}

/// Whether a range `[min, max]` proves `x op v` TRUE for every value `x` in it.
fn must_hold<T: PartialOrd + ?Sized>(op: CmpOp, min: &T, max: &T, v: &T) -> bool {
    match op {
        CmpOp::Eq => min == v && max == v,
        CmpOp::NotEq => v < min || v > max,
        CmpOp::Lt => max < v,
        CmpOp::LtEq => max <= v,
        CmpOp::Gt => min > v,
        CmpOp::GtEq => min >= v,
    }
}

/// Whether the zone of chunk `entry` proves that every row satisfies `term`, an integer
/// comparison or `IS NOT NULL` on the chunk's column (`field`). Only a chunk with a zone and
/// without nulls proves anything; any other term proves nothing.
pub(crate) fn chunk_must_match(term: &Bound, entry: &BlockEntry, field: &Field) -> bool {
    let no_nulls = entry.flags & (flags::HAS_NULLS | flags::ALL_NULL) == 0
        && entry.encoding != Encoding::ConstNull;
    match term {
        Bound::IsNotNull(_) => no_nulls,
        Bound::Cmp(_, op, lit) => {
            if !no_nulls || entry.flags & flags::HAS_ZONE == 0 || lit.is_null(0) {
                return false;
            }
            let int = matches!(
                field.data_type(),
                DataType::Int8
                    | DataType::Int16
                    | DataType::Int32
                    | DataType::Int64
                    | DataType::UInt8
                    | DataType::UInt16
                    | DataType::UInt32
                    | DataType::Date32
                    | DataType::Date64
                    | DataType::Timestamp(_, _)
            );
            match (
                int,
                read_back_i64(lit),
                zone_i64(&entry.min),
                zone_i64(&entry.max),
            ) {
                (true, Some(v), Some(min), Some(max)) => must_hold(*op, &min, &max, &v),
                _ => false,
            }
        }
        _ => false,
    }
}

/// The conjuncts of `b` (`b` itself when it is not an `And`) with the column of each, when
/// every one is a comparison or `IS NOT NULL` on one column: the predicates whose matching rows
/// the chunk zones can prove ([`chunk_must_match`]). `None` for any other predicate.
pub(crate) fn provable_conjuncts(b: &Bound) -> Option<Vec<(usize, &Bound)>> {
    let terms: Vec<&Bound> = match b {
        Bound::And(terms) => terms.iter().collect(),
        other => vec![other],
    };
    terms
        .into_iter()
        .map(|term| match term {
            Bound::Cmp(column, _, _) | Bound::IsNotNull(column) => Some((*column, term)),
            _ => None,
        })
        .collect()
}

/// The column and the values `lo..=hi` that `b` keeps, when `b` is one comparison or a
/// conjunction of comparisons other than `!=` on one integer column (a range filter): its rows
/// can be found from packed values ([`crate::narrow::range_mask`]). `None` for anything else.
/// The bounds may be empty (`lo > hi`): no row matches.
pub(crate) fn int_interval(b: &Bound) -> Option<(usize, i128, i128)> {
    let conjuncts = provable_conjuncts(b)?;
    let (mut lo, mut hi) = (i128::MIN, i128::MAX);
    let mut column = None;
    for (c, term) in conjuncts {
        let Bound::Cmp(_, op, lit) = term else {
            return None;
        };
        if column.is_some_and(|other| other != c) {
            return None;
        }
        column = Some(c);
        let int = matches!(
            lit.data_type(),
            DataType::Int8
                | DataType::Int16
                | DataType::Int32
                | DataType::Int64
                | DataType::UInt8
                | DataType::UInt16
                | DataType::UInt32
                | DataType::Date32
                | DataType::Date64
                | DataType::Timestamp(_, _)
        );
        if !int || lit.is_null(0) {
            return None;
        }
        let v = i128::from(read_back_i64(lit)?);
        match op {
            CmpOp::Eq => {
                lo = lo.max(v);
                hi = hi.min(v);
            }
            CmpOp::Lt => hi = hi.min(v - 1),
            CmpOp::LtEq => hi = hi.min(v),
            CmpOp::Gt => lo = lo.max(v + 1),
            CmpOp::GtEq => lo = lo.max(v),
            CmpOp::NotEq => return None,
        }
    }
    column.map(|c| (c, lo, hi))
}

/// Whether a chunk may hold a row for which the term `value op lit` is TRUE, by its zone.
/// A null literal never matches, and a chunk without a zone may match.
fn term_may_match(entry: &BlockEntry, field: &Field, op: CmpOp, lit: &ArrayRef) -> Result<bool> {
    if lit.is_null(0) {
        return Ok(false);
    }
    if entry.flags & flags::HAS_ZONE == 0 {
        return Ok(true);
    }
    let dt = field.data_type();
    match dt {
        DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::Date32
        | DataType::Date64
        | DataType::Timestamp(_, _) => {
            let (Some(v), Some(min), Some(max)) = (
                read_back_i64(lit),
                zone_i64(&entry.min),
                zone_i64(&entry.max),
            ) else {
                return Ok(true);
            };
            Ok(may_hold(op, &min, &max, &v))
        }
        DataType::Float32 | DataType::Float64 => {
            // A NaN in the chunk is above every number in the total order, so only a chunk
            // without NaN can be pruned. A NaN, zero or missing literal is never pruned.
            if entry.flags & flags::HAS_NAN != 0 {
                return Ok(true);
            }
            let Some(v) = read_back_f64(lit) else {
                return Ok(true);
            };
            if v.is_nan() || v == 0.0 {
                return Ok(true);
            }
            let (Some(min), Some(max)) = (zone_f64(&entry.min), zone_f64(&entry.max)) else {
                return Ok(true);
            };
            Ok(may_hold(op, &min, &max, &v))
        }
        DataType::FixedSizeBinary(width) => {
            let Ok(width) = usize::try_from(*width) else {
                return Ok(true);
            };
            if width > 16 {
                return Ok(true);
            }
            let Some(array) = lit.as_any().downcast_ref::<FixedSizeBinaryArray>() else {
                return Ok(true);
            };
            let (Some(min), Some(max)) = (entry.min.get(..width), entry.max.get(..width)) else {
                return Ok(true);
            };
            Ok(may_hold::<[u8]>(op, min, max, array.value(0)))
        }
        DataType::Utf8 | DataType::Binary if entry.encoding == Encoding::Varbin => {
            let Some(v) = lit_bytes(lit) else {
                return Ok(true);
            };
            if entry.flags & flags::SHARED_PREFIX != 0 {
                return Ok(shared_prefix_may_hold(op, &entry.min, &entry.max, v));
            }
            Ok(prefix_may_hold(op, &entry.min, &entry.max, &zone_prefix(v)))
        }
        _ => Ok(true),
    }
}

/// The bytes of a one-element string or binary literal. `None` for any other type.
fn lit_bytes(lit: &ArrayRef) -> Option<&[u8]> {
    match lit.data_type() {
        DataType::Utf8 => lit.as_string_opt::<i32>().map(|a| a.value(0).as_bytes()),
        DataType::LargeUtf8 => lit.as_string_opt::<i64>().map(|a| a.value(0).as_bytes()),
        DataType::Utf8View => lit.as_string_view_opt().map(|a| a.value(0).as_bytes()),
        DataType::Binary => lit.as_binary_opt::<i32>().map(|a| a.value(0)),
        DataType::LargeBinary => lit.as_binary_opt::<i64>().map(|a| a.value(0)),
        DataType::BinaryView => lit.as_binary_view_opt().map(|a| a.value(0)),
        _ => None,
    }
}

/// The first 16 bytes of `v`, zero-padded: how a `Varbin` zone stores its bounds.
fn zone_prefix(v: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let n = v.len().min(16);
    if let (Some(dst), Some(src)) = (out.get_mut(..n), v.get(..n)) {
        dst.copy_from_slice(src);
    }
    out
}

/// [`may_hold`] on 16-byte prefixes. Prefixes keep the order of the values but not its
/// strictness (two values can share a prefix), so every comparison is taken as non-strict:
/// `x < v` only implies `prefix(x) <= prefix(v)`.
fn prefix_may_hold(op: CmpOp, min: &[u8; 16], max: &[u8; 16], v: &[u8; 16]) -> bool {
    match op {
        CmpOp::Eq => min <= v && v <= max,
        CmpOp::NotEq => true,
        CmpOp::Lt | CmpOp::LtEq => min <= v,
        CmpOp::Gt | CmpOp::GtEq => max >= v,
    }
}

/// [`may_hold`] on a shared-prefix zone ([`flags::SHARED_PREFIX`]). Only equality is pruned:
/// a value equal to `v` starts with the shared prefix, so `v` must be at least as long, its
/// prefix must have the stored hash (a collision only keeps a chunk), and its next 11 bytes,
/// zero-padded, must lie in the zone, non-strictly as in [`prefix_may_hold`].
fn shared_prefix_may_hold(op: CmpOp, min: &[u8; 16], max: &[u8; 16], v: &[u8]) -> bool {
    if op != CmpOp::Eq {
        return true;
    }
    let Some(&shared) = min.first() else {
        return true;
    };
    let shared = usize::from(shared);
    let (Some(prefix), Some(rest)) = (v.get(..shared), v.get(shared..)) else {
        return false;
    };
    if min.get(1..5) != Some(prefix_hash(prefix).as_slice()) {
        return false;
    }
    let mut next = [0u8; SHARED_SUFFIX];
    let n = rest.len().min(SHARED_SUFFIX);
    if let (Some(dst), Some(src)) = (next.get_mut(..n), rest.get(..n)) {
        dst.copy_from_slice(src);
    }
    let (Some(lo), Some(hi)) = (min.get(5..), max.get(5..)) else {
        return true;
    };
    lo <= next.as_slice() && next.as_slice() <= hi
}

/// Whether one conjunct may hold for a row of a chunk. A null chunk never does. A constant chunk
/// is decided exactly by evaluating the conjunct on its one value. Otherwise the zone decides,
/// term by term for an `Or`.
fn conjunct_may_match(
    conjunct: &Bound,
    column: usize,
    entry: &BlockEntry,
    field: &Field,
    dict: Option<&Dictionary>,
) -> Result<bool> {
    if entry.encoding == Encoding::Const {
        let row = crate::narrow::decode_chunk(
            &BlockEntry {
                n_rows: 1,
                ..*entry
            },
            None,
            field,
            dict,
        )?;
        let mut columns: Vec<Option<ArrayRef>> = vec![None; column + 1];
        if let Some(slot) = columns.get_mut(column) {
            *slot = Some(row);
        }
        let mask = evaluate(conjunct, &columns, &[], 1)?;
        return Ok(mask.is_valid(0) && mask.value(0));
    }
    match conjunct {
        Bound::Cmp(_, op, lit) => term_may_match(entry, field, *op, lit),
        Bound::Or(terms) => {
            for term in terms {
                if let Bound::Cmp(_, op, lit) = term
                    && term_may_match(entry, field, *op, lit)?
                {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        _ => Ok(true),
    }
}

/// Whether chunk `entry` of column `column` may hold a row satisfying `b`. It may do so only when
/// every pruning conjunct on `column` may hold. `codes` are the matching dictionary codes of
/// the column, from [`matching_codes`]. `Ok(true)` when nothing is known.
pub(crate) fn chunk_may_match(
    b: &Bound,
    column: usize,
    entry: &BlockEntry,
    field: &Field,
    dict: Option<&Dictionary>,
    codes: Option<&BTreeSet<u32>>,
) -> Result<bool> {
    let terms = pruning_terms(b, column);
    if terms.is_empty() {
        return Ok(true);
    }
    if entry.encoding == Encoding::ConstNull {
        return Ok(false);
    }
    // A dictionary chunk keeps the lowest and highest code of its valid rows in its zone.
    if let Some(codes) = codes
        && entry.encoding == Encoding::Dict
        && entry.flags & flags::HAS_ZONE != 0
        && field.data_type() == &DataType::Utf8
        && let (Some(lo), Some(hi)) = (zone_u32(&entry.min), zone_u32(&entry.max))
        // A zone with lo > hi is corrupt: it is not trusted, and `range` would panic on it.
        && lo <= hi
        && codes.range(lo..=hi).next().is_none()
    {
        return Ok(false);
    }
    for term in terms {
        if !conjunct_may_match(term, column, entry, field, dict)? {
            return Ok(false);
        }
    }
    Ok(true)
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

    #[test]
    fn must_hold_proves_only_ranges_that_satisfy_the_whole_comparison() {
        // Zone [10, 20].
        assert!(must_hold(CmpOp::Eq, &10, &10, &10));
        assert!(!must_hold(CmpOp::Eq, &10, &20, &10));
        assert!(must_hold(CmpOp::NotEq, &10, &20, &9));
        assert!(must_hold(CmpOp::NotEq, &10, &20, &21));
        assert!(!must_hold(CmpOp::NotEq, &10, &20, &15));
        assert!(must_hold(CmpOp::Lt, &10, &20, &21));
        assert!(!must_hold(CmpOp::Lt, &10, &20, &20));
        assert!(must_hold(CmpOp::LtEq, &10, &20, &20));
        assert!(!must_hold(CmpOp::LtEq, &10, &20, &19));
        assert!(must_hold(CmpOp::Gt, &10, &20, &9));
        assert!(!must_hold(CmpOp::Gt, &10, &20, &10));
        assert!(must_hold(CmpOp::GtEq, &10, &20, &10));
        assert!(!must_hold(CmpOp::GtEq, &10, &20, &11));
        // Every value a proof accepts satisfies the comparison, for every zone and literal.
        for op in [
            CmpOp::Eq,
            CmpOp::NotEq,
            CmpOp::Lt,
            CmpOp::LtEq,
            CmpOp::Gt,
            CmpOp::GtEq,
        ] {
            for (lo, hi) in [(0, 0), (0, 5), (3, 8), (5, 5)] {
                for v in -1..10 {
                    if must_hold(op, &lo, &hi, &v) {
                        for x in lo..=hi {
                            let holds = match op {
                                CmpOp::Eq => x == v,
                                CmpOp::NotEq => x != v,
                                CmpOp::Lt => x < v,
                                CmpOp::LtEq => x <= v,
                                CmpOp::Gt => x > v,
                                CmpOp::GtEq => x >= v,
                            };
                            assert!(holds, "{op:?} {x} {v} in [{lo}, {hi}]");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn may_hold_matches_the_pruning_table() {
        // Zone [10, 20].
        assert!(may_hold(CmpOp::Eq, &10, &20, &15));
        assert!(!may_hold(CmpOp::Eq, &10, &20, &9));
        assert!(!may_hold(CmpOp::Eq, &10, &20, &21));
        assert!(!may_hold(CmpOp::Lt, &10, &20, &10));
        assert!(may_hold(CmpOp::Lt, &10, &20, &11));
        assert!(!may_hold(CmpOp::LtEq, &10, &20, &9));
        assert!(may_hold(CmpOp::LtEq, &10, &20, &10));
        assert!(!may_hold(CmpOp::Gt, &10, &20, &20));
        assert!(may_hold(CmpOp::Gt, &10, &20, &19));
        assert!(!may_hold(CmpOp::GtEq, &10, &20, &21));
        assert!(may_hold(CmpOp::GtEq, &10, &20, &20));
        assert!(may_hold(CmpOp::NotEq, &10, &10, &10));
    }

    #[test]
    fn a_varbin_zone_prunes_strings_by_their_16_byte_prefix() {
        // The two longest values share their first 16 bytes and differ after them.
        let values: ArrayRef = Arc::new(StringArray::from(vec![
            Some("ELEC.GEN.ALL-AK-99.A"),
            None,
            Some("ELEC.GEN.ALL-AK-99.Q"),
            Some("ELEC.CONS"),
        ]));
        let chunk =
            crate::narrow::encode_chunk(&values, None, &crate::config::Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::Varbin);
        assert_ne!(chunk.flags & flags::HAS_ZONE, 0);
        assert_eq!(&chunk.min[..9], b"ELEC.CONS");
        assert_eq!(chunk.min[9..], [0u8; 7]);
        assert_eq!(&chunk.max, b"ELEC.GEN.ALL-AK-");
        let entry = BlockEntry {
            first_row: 0,
            n_rows: 4,
            segment_offset: 0,
            segment_len: 0,
            encoding: chunk.encoding,
            flags: chunk.flags,
            const_len: 0,
            min: chunk.min,
            max: chunk.max,
        };
        let field = Field::new("series_id", DataType::Utf8, true);
        let may = |op, v: &str| {
            let lit: ArrayRef = Arc::new(StringArray::from(vec![v]));
            term_may_match(&entry, &field, op, &lit).unwrap()
        };
        // Every value in the chunk may match itself, including one past the stored prefix.
        for v in ["ELEC.CONS", "ELEC.GEN.ALL-AK-99.A", "ELEC.GEN.ALL-AK-99.Q"] {
            assert!(may(CmpOp::Eq, v));
        }
        // A value whose prefix is inside the zone cannot be ruled out.
        assert!(may(CmpOp::Eq, "ELEC.GEN.ALL-AK-99.Z"));
        assert!(may(CmpOp::Eq, "ELEC.D"));
        // Below the minimum or above the maximum prefix.
        assert!(!may(CmpOp::Eq, "ELEC.CON"));
        assert!(!may(CmpOp::Eq, "ELEC.GEN.ALL-AL"));
        assert!(!may(CmpOp::Eq, "PET.X"));
        // Ranges are non-strict on prefixes: a shared prefix never prunes.
        assert!(may(CmpOp::Gt, "ELEC.GEN.ALL-AK-99.Z"));
        assert!(!may(CmpOp::Gt, "ELEC.GEN.ALL-AL"));
        assert!(may(CmpOp::Lt, "ELEC.CONS"));
        assert!(!may(CmpOp::Lt, "ELEC.CON"));
        assert!(may(CmpOp::NotEq, "ELEC.CONS"));
        // A literal of another type is not pruned.
        let int: ArrayRef = Arc::new(Int64Array::from(vec![1]));
        assert!(term_may_match(&entry, &field, CmpOp::Eq, &int).unwrap());
    }

    #[test]
    fn a_shared_prefix_zone_prunes_equality_past_the_first_16_bytes() {
        // Sorted ids sharing 25 bytes: a plain 16-byte zone could not tell these chunks apart.
        let values: ArrayRef = Arc::new(StringArray::from(vec![
            "ELEC.PLANT.AVG_HEAT.1391-DFO-ALL.Q",
            "ELEC.PLANT.AVG_HEAT.1391-NG-ALL.Q",
            "ELEC.PLANT.AVG_HEAT.1391-SUB-ALL.Q",
        ]));
        let chunk =
            crate::narrow::encode_chunk(&values, None, &crate::config::Config::default()).unwrap();
        assert_ne!(chunk.flags & flags::SHARED_PREFIX, 0);
        assert_eq!(chunk.min[0], 25);
        assert_eq!(&chunk.min[5..], b"DFO-ALL.Q\0\0");
        assert_eq!(&chunk.max[5..], b"SUB-ALL.Q\0\0");
        let entry = BlockEntry {
            first_row: 0,
            n_rows: 3,
            segment_offset: 0,
            segment_len: 0,
            encoding: chunk.encoding,
            flags: chunk.flags,
            const_len: 0,
            min: chunk.min,
            max: chunk.max,
        };
        let field = Field::new("series_id", DataType::Utf8, false);
        let may = |op, v: &str| {
            let lit: ArrayRef = Arc::new(StringArray::from(vec![v]));
            term_may_match(&entry, &field, op, &lit).unwrap()
        };
        for v in [
            "ELEC.PLANT.AVG_HEAT.1391-DFO-ALL.Q",
            "ELEC.PLANT.AVG_HEAT.1391-NG-ALL.Q",
            "ELEC.PLANT.AVG_HEAT.1391-SUB-ALL.Q",
            // Between the bounds: cannot be ruled out.
            "ELEC.PLANT.AVG_HEAT.1391-OTHER",
        ] {
            assert!(may(CmpOp::Eq, v), "{v}");
        }
        // Another prefix, a suffix outside the zone, or too short for the prefix.
        assert!(!may(CmpOp::Eq, "ELEC.PLANT.AVG_HEAT.1392-DFO-ALL.Q"));
        assert!(!may(CmpOp::Eq, "ELEC.PLANT.AVG_HEAT.1391-AAA"));
        assert!(!may(CmpOp::Eq, "ELEC.PLANT.AVG_HEAT.1391-ZZZ"));
        assert!(!may(CmpOp::Eq, "ELEC.PLANT"));
        // Only equality prunes in this form.
        assert!(may(CmpOp::Lt, "A"));
        assert!(may(CmpOp::Gt, "Z"));
        // A chunk of one long value repeated is `Varbin` (too long for `Const`), with an empty
        // suffix range.
        let same: ArrayRef = Arc::new(StringArray::from(vec!["ELEC.PLANT.AVG_HEAT.X"; 3]));
        let chunk =
            crate::narrow::encode_chunk(&same, None, &crate::config::Config::default()).unwrap();
        let entry = BlockEntry {
            flags: chunk.flags,
            min: chunk.min,
            max: chunk.max,
            ..entry
        };
        let lit = |v: &str| -> ArrayRef { Arc::new(StringArray::from(vec![v])) };
        assert!(term_may_match(&entry, &field, CmpOp::Eq, &lit("ELEC.PLANT.AVG_HEAT.X")).unwrap());
        assert!(
            !term_may_match(&entry, &field, CmpOp::Eq, &lit("ELEC.PLANT.AVG_HEAT.XY")).unwrap()
        );
    }

    #[test]
    fn a_null_literal_never_matches_a_term() {
        let entry = BlockEntry {
            first_row: 0,
            n_rows: 4,
            segment_offset: 0,
            segment_len: 0,
            encoding: Encoding::ConstNull,
            flags: 0,
            const_len: 0,
            min: [0; 16],
            max: [0; 16],
        };
        let field = Field::new("x", DataType::Int64, true);
        let null: ArrayRef = new_null_array(field.data_type(), 1);
        assert!(!term_may_match(&entry, &field, CmpOp::Eq, &null).unwrap());
    }

    #[test]
    fn a_constant_is_the_same_value_on_every_row() {
        for value in [Some(true), Some(false), None] {
            let mask = evaluate(&Bound::Const(value), &[], &[], 3).unwrap();
            assert_eq!(mask, BooleanArray::from(vec![value; 3]));
        }
    }

    #[test]
    fn a_false_or_null_top_level_conjunct_is_never_true() {
        let and = |terms| Bound::And(terms);
        assert!(never_true(&Bound::Const(Some(false))));
        assert!(never_true(&Bound::Const(None)));
        assert!(never_true(&and(vec![
            Bound::Const(Some(true)),
            Bound::Const(Some(false)),
        ])));
        assert!(!never_true(&Bound::Const(Some(true))));
        // Under an `Or` a constant is not a conjunct, so nothing is proven.
        assert!(!never_true(&Bound::Or(vec![Bound::Const(Some(false))])));
        assert!(!never_true(&Bound::Not(Box::new(Bound::Const(None)))));
    }

    #[test]
    fn a_very_deep_predicate_is_rejected_without_recursion() {
        let mut p = Predicate::IsNull(0);
        for _ in 0..20_000 {
            p = Predicate::Not(Box::new(p));
        }
        match check_size(&p) {
            Err(StrataError::Unsupported { what }) => assert_eq!(what, "predicate too large"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
        // A chain of exactly the depth limit is accepted.
        let mut q = Predicate::IsNull(0);
        for _ in 0..MAX_PREDICATE_DEPTH - 1 {
            q = Predicate::Not(Box::new(q));
        }
        assert!(check_size(&q).is_ok());
        // One more level is rejected.
        q = Predicate::Not(Box::new(q));
        assert!(check_size(&q).is_err());
        // An In with too many literals is rejected by the node count.
        let lits = vec![Lit::Null; MAX_PREDICATE_NODES];
        assert!(check_size(&Predicate::In(0, lits)).is_err());
    }
}
