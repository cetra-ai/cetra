//! Parses dataset imports: CSV with a column mapping and JSONL (D-J-6). Pure functions: no database, no I/O.

use std::fmt;

use csv::ByteRecord;
use serde_json::{Map, Value};

use crate::api::common::FieldError;
use crate::api::datasets::{ImportFormat, ImportMapping, ImportRowError};
use crate::datasets::ItemValues;

/// The largest import file (D-J-6).
pub(crate) const MAX_FILE_BYTES: usize = 100 * 1024 * 1024;
/// The most data rows (CSV records after the header, JSONL non-empty lines) in one import.
pub(crate) const MAX_ROWS: usize = 50_000;
/// The largest preview body (D-J-6).
pub(crate) const MAX_PREVIEW_BYTES: usize = 1024 * 1024;
/// Rows a preview shows.
pub(crate) const PREVIEW_ROWS: usize = 20;
/// Row errors listed in an answer; `error_count` counts all of them.
pub(crate) const MAX_LISTED_ERRORS: usize = 100;
/// Mapped columns at most.
pub(crate) const MAX_COLUMNS: usize = 1_000;
/// The longest key of `input.<key>`, `expected.<key>` or `metadata.<key>`, in characters.
pub(crate) const MAX_KEY_CHARS: usize = 200;

/// Where a column goes, parsed from `ImportColumn::target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// The whole cell is the input.
    Input,
    /// The cell is the value of `input.<key>`.
    InputKey(String),
    /// The whole cell is the expected output.
    Expected,
    /// The cell is the value of `expected.<key>`.
    ExpectedKey(String),
    /// The cell is the value of `metadata.<key>`.
    MetadataKey(String),
    /// The column is not imported.
    Ignore,
}

/// A checked mapping: for each header index, its target (`Ignore` for unlisted columns).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    targets: Vec<Target>,
}

impl fmt::Debug for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Plan")
            .field("columns", &self.targets.len())
            .finish()
    }
}

/// The parsed rows of a whole file: valid items with their row number, and the row errors.
#[derive(Default)]
pub(crate) struct Parsed {
    /// Valid rows in file order: `(row, item)`.
    pub(crate) items: Vec<(i64, ItemValues)>,
    /// Every row error, in file order (the caller lists the first `MAX_LISTED_ERRORS`).
    pub(crate) errors: Vec<ImportRowError>,
}

impl fmt::Debug for Parsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Parsed")
            .field("items", &self.items.len())
            .field("errors", &self.errors.len())
            .finish()
    }
}

/// What a preview shows: the columns and the first rows of the head of a file.
#[derive(Clone, PartialEq)]
pub(crate) struct PreviewRows {
    pub(crate) columns: Vec<String>,
    /// CSV: each row is a JSON array of the raw cell strings. JSONL: each row is the line's value.
    pub(crate) rows: Vec<Value>,
    pub(crate) errors: Vec<ImportRowError>,
    /// More rows follow, or the head was cut (`partial`).
    pub(crate) truncated: bool,
}

impl fmt::Debug for PreviewRows {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreviewRows")
            .field("columns", &self.columns.len())
            .field("rows", &self.rows.len())
            .field("errors", &self.errors.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// A failure of the whole file (a 400), not of one row.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum FileError {
    /// The mapping is invalid. Each entry is a field error with `field = "mapping.columns[<i>]…"`.
    #[error("invalid mapping")]
    Mapping(Vec<FieldError>),
    /// The CSV header is missing, empty, not UTF-8, or has empty or duplicate names.
    #[error("{0}")]
    Header(String),
    /// More than `MAX_ROWS` data rows.
    #[error("the file has more than 50000 rows")]
    TooManyRows,
}

/// Parses one target string. `Err` is the message for the field error.
pub(crate) fn parse_target(text: &str) -> Result<Target, &'static str> {
    const USAGE: &str =
        "must be input, input.<key>, expected, expected.<key>, metadata.<key> or ignore";
    match text {
        "input" => return Ok(Target::Input),
        "expected" => return Ok(Target::Expected),
        "ignore" => return Ok(Target::Ignore),
        _ => {}
    }
    let Some((head, key)) = text.split_once('.') else {
        return Err(USAGE);
    };
    let make: fn(String) -> Target = match head {
        "input" => Target::InputKey,
        "expected" => Target::ExpectedKey,
        "metadata" => Target::MetadataKey,
        _ => return Err(USAGE),
    };
    if key.trim().is_empty() {
        return Err(USAGE);
    }
    if key.chars().count() > MAX_KEY_CHARS {
        return Err("the key is longer than 200 characters");
    }
    Ok(make(key.to_owned()))
}

/// Checks `mapping` against the CSV header and returns the target of every column.
pub(crate) fn plan(header: &[String], mapping: &ImportMapping) -> Result<Plan, FileError> {
    if mapping.columns.len() > MAX_COLUMNS {
        return Err(FileError::Mapping(vec![field_error(
            "mapping.columns",
            "at most 1000 columns",
        )]));
    }
    let mut errors = Vec::new();
    let mut targets = vec![Target::Ignore; header.len()];
    let mut listed = vec![false; header.len()];
    for (i, entry) in mapping.columns.iter().enumerate() {
        let column_field = format!("mapping.columns[{i}].column");
        let target_field = format!("mapping.columns[{i}].target");
        let Some(index) = header.iter().position(|name| *name == entry.column) else {
            errors.push(field_error(&column_field, "no such column"));
            continue;
        };
        if listed[index] {
            errors.push(field_error(&column_field, "listed twice"));
            continue;
        }
        let target = match parse_target(&entry.target) {
            Ok(target) => target,
            Err(message) => {
                errors.push(field_error(&target_field, message));
                continue;
            }
        };
        if target != Target::Ignore && targets.contains(&target) {
            errors.push(field_error(&target_field, "another column has this target"));
            continue;
        }
        listed[index] = true;
        targets[index] = target;
    }
    let whole_input = targets.contains(&Target::Input);
    let keyed_input = targets.iter().any(|t| matches!(t, Target::InputKey(_)));
    let whole_expected = targets.contains(&Target::Expected);
    let keyed_expected = targets.iter().any(|t| matches!(t, Target::ExpectedKey(_)));
    if whole_input && keyed_input {
        errors.push(field_error(
            "mapping.columns",
            "input cannot be both whole and keyed",
        ));
    }
    if whole_expected && keyed_expected {
        errors.push(field_error(
            "mapping.columns",
            "expected cannot be both whole and keyed",
        ));
    }
    if !whole_input && !keyed_input {
        errors.push(field_error("mapping.columns", "no column maps to input"));
    }
    if errors.is_empty() {
        Ok(Plan { targets })
    } else {
        Err(FileError::Mapping(errors))
    }
}

/// Parses a whole CSV file with a mapping. `bytes` is the file as uploaded.
pub(crate) fn parse_csv(bytes: &[u8], mapping: &ImportMapping) -> Result<Parsed, FileError> {
    let mut reader = csv_reader(strip_bom(bytes));
    let mut records = reader.byte_records();
    let header = header_names(records.next())?;
    let checked = plan(&header, mapping)?;
    let mut parsed = Parsed::default();
    let mut rows = 0usize;
    for record in records {
        rows += 1;
        if rows > MAX_ROWS {
            return Err(FileError::TooManyRows);
        }
        let row = i64::try_from(rows).unwrap_or(i64::MAX);
        let item = match record {
            Ok(record) => csv_item(&record, &checked),
            Err(_) => Err("the row cannot be read".to_owned()),
        };
        match item {
            Ok(item) => parsed.items.push((row, item)),
            Err(message) => parsed.errors.push(row_error(row, &message)),
        }
    }
    Ok(parsed)
}

/// Parses a whole JSONL file. Each non-empty line is one item object.
pub(crate) fn parse_jsonl(bytes: &[u8]) -> Result<Parsed, FileError> {
    let mut parsed = Parsed::default();
    let mut rows = 0usize;
    for (index, line) in strip_bom(bytes).split(|&byte| byte == b'\n').enumerate() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        rows += 1;
        if rows > MAX_ROWS {
            return Err(FileError::TooManyRows);
        }
        let row = i64::try_from(index + 1).unwrap_or(i64::MAX);
        match jsonl_item(line) {
            Ok(item) => parsed.items.push((row, item)),
            Err(message) => parsed.errors.push(row_error(row, &message)),
        }
    }
    Ok(parsed)
}

/// Parses the head of a file. With `partial`, the bytes are the first part of a larger file:
/// the last record (CSV) or line (JSONL) may be cut, so it is dropped.
pub(crate) fn preview(
    bytes: &[u8],
    format: ImportFormat,
    partial: bool,
) -> Result<PreviewRows, FileError> {
    let bytes = strip_bom(bytes);
    match format {
        ImportFormat::Csv => preview_csv(bytes, partial),
        ImportFormat::Jsonl => Ok(preview_jsonl(bytes, partial)),
    }
}

/// A CSV cell as a JSON value: a cell whose trimmed text starts with `{` or `[` and parses as
/// JSON becomes that value; any other non-empty cell is the cell text as a string (untrimmed).
/// An empty cell (length 0) is `None`: its key is left out.
fn cell_value(cell: &str) -> Option<Value> {
    if cell.is_empty() {
        return None;
    }
    let trimmed = cell.trim();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(value) = serde_json::from_str::<Value>(trimmed)
    {
        return Some(value);
    }
    Some(Value::String(cell.to_owned()))
}

/// Sets `key` to the cell's value, unless the cell is empty.
fn set_key(map: &mut Map<String, Value>, key: &str, cell: &str) {
    if let Some(value) = cell_value(cell) {
        map.insert(key.to_owned(), value);
    }
}

/// One CSV record as an item. `Err` is the row error message.
fn csv_item(record: &ByteRecord, checked: &Plan) -> Result<ItemValues, String> {
    if record.len() != checked.targets.len() {
        return Err(format!(
            "the row has {} fields, the header has {}",
            record.len(),
            checked.targets.len()
        ));
    }
    let cells: Vec<&str> = record
        .iter()
        .map(std::str::from_utf8)
        .collect::<Result<_, _>>()
        .map_err(|_| "the row is not valid UTF-8".to_owned())?;
    let mut whole_input = None;
    let mut input_keys = Map::new();
    let mut whole_expected = None;
    let mut expected_keys = Map::new();
    let mut metadata = Map::new();
    for (target, cell) in checked.targets.iter().zip(cells) {
        match target {
            Target::Ignore => {}
            Target::Input => whole_input = cell_value(cell),
            Target::InputKey(key) => set_key(&mut input_keys, key, cell),
            Target::Expected => whole_expected = cell_value(cell),
            Target::ExpectedKey(key) => set_key(&mut expected_keys, key, cell),
            Target::MetadataKey(key) => set_key(&mut metadata, key, cell),
        }
    }
    let input = match whole_input {
        Some(value) => value,
        None if !input_keys.is_empty() => Value::Object(input_keys),
        None => return Err("the row has no input".to_owned()),
    };
    let expected_output = match whole_expected {
        Some(value) => Some(value),
        None if !expected_keys.is_empty() => Some(Value::Object(expected_keys)),
        None => None,
    };
    Ok(ItemValues {
        input,
        expected_output,
        metadata,
    })
}

/// One JSONL line as an item. `Err` is the row error message.
fn jsonl_item(line: &[u8]) -> Result<ItemValues, String> {
    let text = std::str::from_utf8(line).map_err(|_| "the line is not valid UTF-8".to_owned())?;
    let value: Value =
        serde_json::from_str(text).map_err(|_| "the line is not valid JSON".to_owned())?;
    let Value::Object(mut object) = value else {
        return Err("the line is not a JSON object".to_owned());
    };
    if let Some(key) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "input" | "expected_output" | "metadata"))
    {
        return Err(format!("unknown key {key}"));
    }
    let input = match object.remove("input") {
        None | Some(Value::Null) => return Err("the line has no input".to_owned()),
        Some(input) => input,
    };
    let expected_output = match object.remove("expected_output") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value),
    };
    let metadata = match object.remove("metadata") {
        None => Map::new(),
        Some(Value::Object(metadata)) => metadata,
        Some(_) => return Err("metadata must be an object".to_owned()),
    };
    Ok(ItemValues {
        input,
        expected_output,
        metadata,
    })
}

/// The header record as column names. Names are non-empty and unique.
fn header_names(record: Option<Result<ByteRecord, csv::Error>>) -> Result<Vec<String>, FileError> {
    let record = match record {
        None => return Err(FileError::Header("the file has no header".to_owned())),
        Some(Err(_)) => return Err(FileError::Header("the header cannot be read".to_owned())),
        Some(Ok(record)) => record,
    };
    let mut names: Vec<String> = Vec::with_capacity(record.len());
    for (index, field) in record.iter().enumerate() {
        let number = index + 1;
        let name = std::str::from_utf8(field)
            .map_err(|_| FileError::Header("the header is not valid UTF-8".to_owned()))?;
        if name.trim().is_empty() {
            return Err(FileError::Header(format!(
                "column {number} has an empty name"
            )));
        }
        if let Some(earlier) = names.iter().position(|known| known == name) {
            return Err(FileError::Header(format!(
                "column {number} repeats the name of column {}",
                earlier + 1
            )));
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

/// The head of a CSV file: the header, then up to `PREVIEW_ROWS` rows as raw cell strings.
fn preview_csv(bytes: &[u8], partial: bool) -> Result<PreviewRows, FileError> {
    let mut reader = csv_reader(bytes);
    // The header, one row past the preview (to see that more follow), and one more record
    // that a partial head drops; the records beyond that are not needed.
    let mut records: Vec<Result<ByteRecord, csv::Error>> =
        reader.byte_records().take(PREVIEW_ROWS + 3).collect();
    if partial && records.len() > 1 {
        records.pop();
    }
    let mut records = records.into_iter();
    let columns = header_names(records.next())?;
    let data: Vec<Result<ByteRecord, csv::Error>> = records.collect();
    let truncated = partial || data.len() > PREVIEW_ROWS;
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for (index, record) in data.into_iter().take(PREVIEW_ROWS).enumerate() {
        let row = i64::try_from(index + 1).unwrap_or(i64::MAX);
        match record {
            Err(_) => errors.push(row_error(row, "the row cannot be read")),
            Ok(record) => {
                match record
                    .iter()
                    .map(std::str::from_utf8)
                    .collect::<Result<Vec<&str>, _>>()
                {
                    Ok(cells) => rows.push(Value::Array(
                        cells
                            .into_iter()
                            .map(|cell| Value::String(cell.to_owned()))
                            .collect(),
                    )),
                    Err(_) => errors.push(row_error(row, "the row is not valid UTF-8")),
                }
            }
        }
    }
    Ok(PreviewRows {
        columns,
        rows,
        errors,
        truncated,
    })
}

/// The head of a JSONL file: up to `PREVIEW_ROWS` non-empty lines, each one JSON value or a
/// row error. The columns are the distinct top-level keys of the object rows.
fn preview_jsonl(bytes: &[u8], partial: bool) -> PreviewRows {
    let mut lines: Vec<&[u8]> = bytes.split(|&byte| byte == b'\n').collect();
    if partial {
        // The text after the last newline is the cut line; when it is empty (the head ends with
        // a newline), the last non-empty line is dropped instead.
        let cut = lines.last().is_some_and(|last| !last.is_empty());
        if cut {
            lines.pop();
        } else {
            lines.pop();
            while lines
                .last()
                .is_some_and(|line| line.iter().all(u8::is_ascii_whitespace))
            {
                lines.pop();
            }
            lines.pop();
        }
    }
    let mut columns: Vec<String> = Vec::new();
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    let mut shown = 0usize;
    let mut more = false;
    for (index, &line) in lines.iter().enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if shown == PREVIEW_ROWS {
            more = true;
            break;
        }
        shown += 1;
        let row = i64::try_from(index + 1).unwrap_or(i64::MAX);
        match std::str::from_utf8(line)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
        {
            None => errors.push(row_error(row, "the line is not valid JSON")),
            Some(value) => {
                if let Value::Object(object) = &value {
                    for key in object.keys() {
                        if !columns.contains(key) {
                            columns.push(key.clone());
                        }
                    }
                }
                rows.push(value);
            }
        }
    }
    PreviewRows {
        columns,
        rows,
        errors,
        truncated: partial || more,
    }
}

fn csv_reader(bytes: &[u8]) -> csv::Reader<&[u8]> {
    csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(bytes)
}

fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes)
}

fn row_error(row: i64, message: &str) -> ImportRowError {
    ImportRowError {
        row,
        message: message.to_owned(),
    }
}

fn field_error(field: &str, message: &str) -> FieldError {
    FieldError {
        field: field.to_owned(),
        message: message.to_owned(),
        position: None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::api::datasets::ImportColumn;

    fn mapping(pairs: &[(&str, &str)]) -> ImportMapping {
        ImportMapping {
            columns: pairs
                .iter()
                .map(|(column, target)| ImportColumn {
                    column: column.to_string(),
                    target: target.to_string(),
                })
                .collect(),
        }
    }

    fn metadata(item: &ItemValues) -> Value {
        Value::Object(item.metadata.clone())
    }

    fn field_messages(errors: &[FieldError]) -> Vec<(&str, &str)> {
        errors
            .iter()
            .map(|e| (e.field.as_str(), e.message.as_str()))
            .collect()
    }

    fn row_messages(errors: &[ImportRowError]) -> Vec<(i64, &str)> {
        errors.iter().map(|e| (e.row, e.message.as_str())).collect()
    }

    #[test]
    fn limits_match_the_design() {
        assert_eq!(MAX_FILE_BYTES, 100 * 1024 * 1024);
        assert_eq!(MAX_ROWS, 50_000);
        assert_eq!(MAX_PREVIEW_BYTES, 1024 * 1024);
        assert_eq!(PREVIEW_ROWS, 20);
        assert_eq!(MAX_LISTED_ERRORS, 100);
        assert_eq!(MAX_COLUMNS, 1_000);
        assert_eq!(MAX_KEY_CHARS, 200);
    }

    #[test]
    fn targets_parse() {
        assert_eq!(parse_target("input"), Ok(Target::Input));
        assert_eq!(parse_target("expected"), Ok(Target::Expected));
        assert_eq!(parse_target("ignore"), Ok(Target::Ignore));
        assert_eq!(
            parse_target("input.q"),
            Ok(Target::InputKey("q".to_owned()))
        );
        assert_eq!(
            parse_target("expected.a"),
            Ok(Target::ExpectedKey("a".to_owned()))
        );
        assert_eq!(
            parse_target("metadata.t"),
            Ok(Target::MetadataKey("t".to_owned()))
        );
        let usage =
            "must be input, input.<key>, expected, expected.<key>, metadata.<key> or ignore";
        for bad in ["metadata", "input.", "input.   ", "foo", "output.a", ""] {
            assert_eq!(parse_target(bad), Err(usage), "{bad:?}");
        }
        let long = format!("input.{}", "x".repeat(201));
        assert_eq!(
            parse_target(&long),
            Err("the key is longer than 200 characters")
        );
        let exact = format!("input.{}", "x".repeat(200));
        assert!(parse_target(&exact).is_ok());
    }

    #[test]
    fn keys_may_hold_dots() {
        assert_eq!(
            parse_target("input.a.b"),
            Ok(Target::InputKey("a.b".to_owned()))
        );
    }

    #[test]
    fn csv_whole_and_keyed_columns() {
        let parsed = parse_csv(
            b"q,a,topic\nhi,there,billing\n",
            &mapping(&[
                ("q", "input"),
                ("a", "expected"),
                ("topic", "metadata.topic"),
            ]),
        )
        .expect("parses");
        assert!(parsed.errors.is_empty());
        let (row, item) = &parsed.items[0];
        assert_eq!(*row, 1);
        assert_eq!(item.input, json!("hi"));
        assert_eq!(item.expected_output, Some(json!("there")));
        assert_eq!(metadata(item), json!({"topic": "billing"}));
    }

    #[test]
    fn csv_keyed_input() {
        let parsed = parse_csv(
            b"q,lang\nhi,en\n",
            &mapping(&[("q", "input.q"), ("lang", "input.lang")]),
        )
        .expect("parses");
        assert_eq!(parsed.items[0].1.input, json!({"q": "hi", "lang": "en"}));
        assert_eq!(parsed.items[0].1.expected_output, None);
    }

    #[test]
    fn csv_json_cells() {
        let text = r#"q,a,b,c,d,e
hi,"{""a"":1}"," [1,2] ",{oops,42,true
"#;
        let parsed = parse_csv(
            text.as_bytes(),
            &mapping(&[
                ("q", "input"),
                ("a", "metadata.a"),
                ("b", "metadata.b"),
                ("c", "metadata.c"),
                ("d", "metadata.d"),
                ("e", "metadata.e"),
            ]),
        )
        .expect("parses");
        assert!(parsed.errors.is_empty());
        assert_eq!(
            metadata(&parsed.items[0].1),
            json!({"a": {"a": 1}, "b": [1, 2], "c": "{oops", "d": "42", "e": "true"})
        );
    }

    #[test]
    fn csv_empty_cells_drop_keys() {
        let parsed = parse_csv(
            b"q,c,t\nhi,,\n",
            &mapping(&[("q", "input.q"), ("c", "input.c"), ("t", "metadata.t")]),
        )
        .expect("parses");
        assert_eq!(parsed.items[0].1.input, json!({"q": "hi"}));
        assert_eq!(metadata(&parsed.items[0].1), json!({}));
    }

    #[test]
    fn csv_row_without_input() {
        let parsed = parse_csv(
            b"q,a\nhi,x\n,y\nho,z\n",
            &mapping(&[("q", "input"), ("a", "expected")]),
        )
        .expect("parses");
        assert_eq!(
            row_messages(&parsed.errors),
            vec![(2, "the row has no input")]
        );
        let rows: Vec<i64> = parsed.items.iter().map(|(row, _)| *row).collect();
        assert_eq!(rows, vec![1, 3]);
    }

    #[test]
    fn csv_quoted_newline_is_one_row() {
        let text = r#"q,a
"line1
line2",x
y,z
"#;
        let parsed = parse_csv(
            text.as_bytes(),
            &mapping(&[("q", "input"), ("a", "expected")]),
        )
        .expect("parses");
        assert_eq!(parsed.items.len(), 2);
        assert_eq!(parsed.items[0].0, 1);
        assert_eq!(parsed.items[0].1.input, json!("line1\nline2"));
        assert_eq!(parsed.items[1].0, 2);
        assert_eq!(parsed.items[1].1.input, json!("y"));
    }

    #[test]
    fn csv_bom_is_stripped() {
        let parsed = parse_csv(
            "\u{feff}q,a\nhi,x\n".as_bytes(),
            &mapping(&[("q", "input")]),
        )
        .expect("parses");
        assert_eq!(parsed.items.len(), 1);
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn csv_crlf() {
        let parsed = parse_csv(b"q\r\nhi\r\n", &mapping(&[("q", "input")])).expect("parses");
        assert_eq!(parsed.items[0].1.input, json!("hi"));
    }

    #[test]
    fn csv_duplicate_header() {
        let error = parse_csv(b"q,a,q\n", &mapping(&[("q", "input")])).expect_err("duplicate");
        assert_eq!(
            error,
            FileError::Header("column 3 repeats the name of column 1".to_owned())
        );
    }

    #[test]
    fn csv_empty_header_name() {
        let error = parse_csv(b"q,,a\n", &mapping(&[("q", "input")])).expect_err("empty");
        assert_eq!(
            error,
            FileError::Header("column 2 has an empty name".to_owned())
        );
    }

    #[test]
    fn csv_empty_file() {
        let error = parse_csv(b"", &mapping(&[("q", "input")])).expect_err("no header");
        assert_eq!(
            error,
            FileError::Header("the file has no header".to_owned())
        );
        let error = preview(b"", ImportFormat::Csv, false).expect_err("no header");
        assert_eq!(
            error,
            FileError::Header("the file has no header".to_owned())
        );
    }

    #[test]
    fn csv_wrong_field_count() {
        let parsed =
            parse_csv(b"q,a\nhi,x,y\nho,z\n", &mapping(&[("q", "input")])).expect("parses");
        assert_eq!(
            row_messages(&parsed.errors),
            vec![(1, "the row has 3 fields, the header has 2")]
        );
        assert_eq!(parsed.items.len(), 1);
        assert_eq!(parsed.items[0].0, 2);
    }

    #[test]
    fn csv_invalid_utf8_row() {
        let parsed = parse_csv(b"q\nhi\n\xFF\nho\n", &mapping(&[("q", "input")])).expect("parses");
        assert_eq!(
            row_messages(&parsed.errors),
            vec![(2, "the row is not valid UTF-8")]
        );
        let rows: Vec<i64> = parsed.items.iter().map(|(row, _)| *row).collect();
        assert_eq!(rows, vec![1, 3]);
    }

    #[test]
    fn mapping_errors_are_collected() {
        let text = b"q,a,b,c\nhi,x,y,z\n";
        let error = parse_csv(
            text,
            &mapping(&[
                ("zz", "input"),
                ("q", "input"),
                ("q", "expected"),
                ("a", "output.a"),
                ("b", "input.x"),
                ("c", "input.x"),
            ]),
        )
        .expect_err("mapping");
        let errors = match error {
            FileError::Mapping(errors) => errors,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(
            field_messages(&errors),
            vec![
                ("mapping.columns[0].column", "no such column"),
                ("mapping.columns[2].column", "listed twice"),
                (
                    "mapping.columns[3].target",
                    "must be input, input.<key>, expected, expected.<key>, metadata.<key> or ignore"
                ),
                (
                    "mapping.columns[5].target",
                    "another column has this target"
                ),
                ("mapping.columns", "input cannot be both whole and keyed"),
            ]
        );
    }

    #[test]
    fn mapping_needs_an_input_and_no_mixed_expected() {
        let error = parse_csv(b"q,a\nhi,x\n", &mapping(&[("a", "expected")])).expect_err("input");
        let FileError::Mapping(errors) = error else {
            panic!("expected a mapping error");
        };
        assert_eq!(
            field_messages(&errors),
            vec![("mapping.columns", "no column maps to input")]
        );

        let error = parse_csv(
            b"q,a,b\nhi,x,y\n",
            &mapping(&[("q", "input"), ("a", "expected"), ("b", "expected.k")]),
        )
        .expect_err("expected");
        let FileError::Mapping(errors) = error else {
            panic!("expected a mapping error");
        };
        assert_eq!(
            field_messages(&errors),
            vec![("mapping.columns", "expected cannot be both whole and keyed")]
        );
    }

    #[test]
    fn plan_rejects_more_than_1000_columns() {
        let header = vec!["q".to_owned()];
        let big = ImportMapping {
            columns: (0..=MAX_COLUMNS)
                .map(|_| ImportColumn {
                    column: "q".to_owned(),
                    target: "input".to_owned(),
                })
                .collect(),
        };
        let error = plan(&header, &big).expect_err("too many");
        assert_eq!(
            error,
            FileError::Mapping(vec![field_error("mapping.columns", "at most 1000 columns")])
        );
    }

    #[test]
    fn csv_too_many_rows() {
        let mut text = String::from("q\n");
        for _ in 0..=MAX_ROWS {
            text.push_str("x\n");
        }
        let error = parse_csv(text.as_bytes(), &mapping(&[("q", "input")])).expect_err("too many");
        assert_eq!(error, FileError::TooManyRows);
    }

    #[test]
    fn csv_exactly_max_rows() {
        let mut text = String::from("q\n");
        for _ in 0..MAX_ROWS {
            text.push_str("x\n");
        }
        let parsed = parse_csv(text.as_bytes(), &mapping(&[("q", "input")])).expect("parses");
        assert_eq!(parsed.items.len(), MAX_ROWS);
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn jsonl_rows() {
        let text = r#"{"input":"a","expected_output":"b","metadata":{"k":1}}
{"input":[1],"metadata":{}}
{"input":"c","expected_output":null}
"#;
        let parsed = parse_jsonl(text.as_bytes()).expect("parses");
        assert!(parsed.errors.is_empty());
        assert_eq!(parsed.items.len(), 3);
        assert_eq!(parsed.items[0].1.expected_output, Some(json!("b")));
        assert_eq!(metadata(&parsed.items[0].1), json!({"k": 1}));
        assert_eq!(parsed.items[1].1.input, json!([1]));
        assert_eq!(parsed.items[2].1.expected_output, None);
    }

    #[test]
    fn jsonl_skips_blank_lines_and_numbers_by_line() {
        let text = "{\"input\":1}\n\n[1]\n{\"input\":4}\n";
        let parsed = parse_jsonl(text.as_bytes()).expect("parses");
        let rows: Vec<i64> = parsed.items.iter().map(|(row, _)| *row).collect();
        assert_eq!(rows, vec![1, 4]);
        assert_eq!(
            row_messages(&parsed.errors),
            vec![(3, "the line is not a JSON object")]
        );
    }

    #[test]
    fn jsonl_unknown_key() {
        let parsed = parse_jsonl(br#"{"input":1,"id":"x"}"#).expect("parses");
        assert!(parsed.items.is_empty());
        assert_eq!(row_messages(&parsed.errors), vec![(1, "unknown key id")]);
    }

    #[test]
    fn jsonl_missing_or_null_input() {
        let text = "{\"expected_output\":1}\n{\"input\":null}\n";
        let parsed = parse_jsonl(text.as_bytes()).expect("parses");
        assert_eq!(
            row_messages(&parsed.errors),
            vec![(1, "the line has no input"), (2, "the line has no input")]
        );
    }

    #[test]
    fn jsonl_bad_json_and_metadata() {
        let text = "{oops\n{\"input\":1,\"metadata\":[1]}\n";
        let parsed = parse_jsonl(text.as_bytes()).expect("parses");
        assert_eq!(
            row_messages(&parsed.errors),
            vec![
                (1, "the line is not valid JSON"),
                (2, "metadata must be an object")
            ]
        );
    }

    #[test]
    fn jsonl_too_many_rows() {
        let text = "{\"input\":1}\n".repeat(MAX_ROWS + 1);
        let error = parse_jsonl(text.as_bytes()).expect_err("too many");
        assert_eq!(error, FileError::TooManyRows);
    }

    #[test]
    fn preview_csv_partial_drops_the_cut_record() {
        let preview = preview(b"q,a\nhi,x\nho,y", ImportFormat::Csv, true).expect("preview");
        assert_eq!(preview.columns, vec!["q", "a"]);
        assert_eq!(preview.rows, vec![json!(["hi", "x"])]);
        assert!(preview.errors.is_empty());
        assert!(preview.truncated);
    }

    #[test]
    fn preview_csv_partial_drops_an_open_quote() {
        let preview = preview(b"q,a\nhi,x\n\"open,y", ImportFormat::Csv, true).expect("preview");
        assert_eq!(preview.rows, vec![json!(["hi", "x"])]);
        assert!(preview.errors.is_empty());
        assert!(preview.truncated);
    }

    #[test]
    fn preview_csv_partial_drops_a_complete_last_record() {
        let preview = preview(b"q,a\nhi,x\nho,y\n", ImportFormat::Csv, true).expect("preview");
        assert_eq!(preview.rows, vec![json!(["hi", "x"])]);
    }

    #[test]
    fn preview_csv_limits_rows() {
        let text = format!(
            "q\n{}",
            (0..25).map(|i| format!("r{i}\n")).collect::<String>()
        );
        let preview = preview(text.as_bytes(), ImportFormat::Csv, false).expect("preview");
        assert_eq!(preview.rows.len(), PREVIEW_ROWS);
        assert_eq!(preview.rows[0], json!(["r0"]));
        assert!(preview.truncated);
    }

    #[test]
    fn preview_jsonl_columns_in_first_seen_order() {
        let text = "{\"input\":1,\"metadata\":{}}\n{\"expected_output\":2,\"input\":3}\n";
        let preview = preview(text.as_bytes(), ImportFormat::Jsonl, false).expect("preview");
        assert_eq!(
            preview.columns,
            vec!["input", "metadata", "expected_output"]
        );
        assert_eq!(preview.rows.len(), 2);
        assert!(!preview.truncated);
    }

    #[test]
    fn preview_jsonl_partial_drops_the_last_line() {
        let text = "{\"input\":1}\n{\"input\":2}\n{\"inp";
        let preview = preview(text.as_bytes(), ImportFormat::Jsonl, true).expect("preview");
        assert_eq!(preview.rows, vec![json!({"input": 1}), json!({"input": 2})]);
        assert!(preview.errors.is_empty());
        assert!(preview.truncated);
    }

    #[test]
    fn preview_jsonl_bad_line() {
        let text = "{\"input\":1}\n{oops\n{\"input\":3}\n";
        let preview = preview(text.as_bytes(), ImportFormat::Jsonl, false).expect("preview");
        assert_eq!(preview.rows, vec![json!({"input": 1}), json!({"input": 3})]);
        assert_eq!(
            row_messages(&preview.errors),
            vec![(2, "the line is not valid JSON")]
        );
    }

    #[test]
    fn debug_prints_no_content() {
        let parsed = parse_jsonl(br#"{"input":"canary-import-1"}"#).expect("parses");
        assert_eq!(parsed.items.len(), 1);
        let text = format!("{parsed:?}");
        assert!(!text.contains("canary-import-1"));
        let preview = preview(
            br#"{"input":"canary-import-2"}"#,
            ImportFormat::Jsonl,
            false,
        )
        .expect("preview");
        let text = format!("{preview:?}");
        assert!(!text.contains("canary-import-2"));
    }
}
