//! Output rules of the assistant's tools: MCP's (D-H-11), applied to API values (D-AS-5).

use serde_json::{Map, Value};

use crate::mcp::output::{self, DEFAULT_MAX_CHARS, char_len, fit_list, render};

/// The character budget of one tool result (MCP's default, D-H-10).
pub(crate) const MAX_CHARS: usize = DEFAULT_MAX_CHARS;
/// Strings longer than this are cut in read results (MCP's `VALUE_CUT`).
pub(crate) const VALUE_CUT: usize = 200;
/// The cut for tools that return configuration the model may redraft (D-AS-19).
pub(crate) const CONFIG_CUT: usize = 4_000;

/// The smallest cut `object_body` falls back to.
const FLOOR_CUT: usize = 50;

/// Cuts every string inside `value`, at any depth (object values and array items; never object
/// keys), to `max` characters with `crate::mcp::output::cut`. Returns true when anything was cut.
pub(crate) fn cut_strings(value: &mut Value, max: usize) -> bool {
    match value {
        Value::String(text) => match output::cut(text, max) {
            Some(cut) => {
                *text = cut;
                true
            }
            None => false,
        },
        Value::Array(items) => {
            let mut any = false;
            for item in items {
                any |= cut_strings(item, max);
            }
            any
        }
        Value::Object(map) => cut_map(map, max),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

/// [`cut_strings`] over the members of an object.
fn cut_map(map: &mut Map<String, Value>, max: usize) -> bool {
    let mut any = false;
    for item in map.values_mut() {
        any |= cut_strings(item, max);
    }
    any
}

fn cut_note(cut: usize) -> String {
    format!("string values longer than {cut} characters were cut to {cut} characters")
}

/// The size of a result body in characters, as the model reads it.
fn size(body: &Map<String, Value>) -> usize {
    char_len(&render(&Value::Object(body.clone())))
}

/// Appends `note` to the object's `notes` array, creating the array when it is missing.
fn push_note(body: &mut Map<String, Value>, note: String) {
    let notes = body
        .entry("notes")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Value::Array(notes) = notes {
        notes.push(Value::String(note));
    }
}

/// Builds `{<list>: items, "next_cursor": cursor, "truncated": false, "notes": []}`. It cuts the strings
/// of the items only (never `next_cursor` or `notes`) to `cut`, adding the note "string values longer than
/// {cut} characters were cut to {cut} characters", then fits the body to [`MAX_CHARS`] with
/// `crate::mcp::output::fit_list` (note: "the list was cut to fit {MAX_CHARS} characters; ask for fewer rows").
/// `truncated` is true when there is a cursor, or when the list was cut.
pub(crate) fn list_body(
    list: &str,
    mut items: Vec<Value>,
    cursor: Option<String>,
    cut: usize,
) -> Value {
    let mut notes = Vec::new();
    let mut cut_any = false;
    for item in &mut items {
        cut_any |= cut_strings(item, cut);
    }
    if cut_any {
        notes.push(Value::String(cut_note(cut)));
    }
    let truncated = cursor.is_some();
    let mut body = Map::new();
    body.insert(list.to_owned(), Value::Array(items));
    body.insert(
        "next_cursor".to_owned(),
        cursor.map_or(Value::Null, Value::String),
    );
    body.insert("truncated".to_owned(), Value::Bool(truncated));
    body.insert("notes".to_owned(), Value::Array(notes));
    fit_list(
        &mut body,
        list,
        MAX_CHARS,
        &format!("the list was cut to fit {MAX_CHARS} characters; ask for fewer rows"),
    );
    Value::Object(body)
}

/// One object result. `value` must be a JSON object (callers serialize a struct); any other value is
/// returned unchanged. It cuts the object's strings to `cut`; when `list` names an array member
/// (`turns`, `series`, `rows`) it then fits that array with `fit_list`; when the compact JSON is still
/// longer than [`MAX_CHARS`], it re-cuts with `cut/2`, `cut/4`, … down to 50 characters, then adds the
/// note "the result is larger than {MAX_CHARS} characters even with values cut to 50 characters" and keeps
/// it. Adds `"truncated"` and `"notes"` keys.
///
/// `truncated` is the object's own `truncated` (false when absent) OR anything was cut or dropped; an
/// existing `notes` array is extended.
pub(crate) fn object_body(value: Value, cut: usize, list: Option<&str>) -> Value {
    let mut body = match value {
        Value::Object(map) => map,
        other => return other,
    };
    let own_truncated = body
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    body.entry("truncated")
        .or_insert(Value::Bool(own_truncated));
    body.entry("notes")
        .or_insert_with(|| Value::Array(Vec::new()));

    let mut cut_any = cut_map(&mut body, cut);
    if cut_any {
        push_note(&mut body, cut_note(cut));
    }
    let fitted = list.is_some_and(|name| {
        fit_list(
            &mut body,
            name,
            MAX_CHARS,
            &format!("the list was cut to fit {MAX_CHARS} characters"),
        )
    });

    let mut limit = cut;
    while size(&body) > MAX_CHARS && limit > FLOOR_CUT {
        limit = (limit / 2).max(FLOOR_CUT);
        if cut_map(&mut body, limit) {
            cut_any = true;
            push_note(&mut body, cut_note(limit));
        }
    }
    if size(&body) > MAX_CHARS {
        push_note(
            &mut body,
            format!(
                "the result is larger than {MAX_CHARS} characters even with values cut to {FLOOR_CUT} characters"
            ),
        );
    }
    body.insert(
        "truncated".to_owned(),
        Value::Bool(own_truncated || cut_any || fitted),
    );
    Value::Object(body)
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};

    use super::{CONFIG_CUT, MAX_CHARS, VALUE_CUT, cut_strings, list_body, object_body};
    use crate::mcp::output::{char_len, render};

    #[test]
    fn the_cuts_are_the_documented_ones() {
        assert_eq!((VALUE_CUT, CONFIG_CUT, MAX_CHARS), (200, 4_000, 20_000));
    }

    #[test]
    fn cut_strings_walks_nested_values() {
        let mut value = json!({
            "outer": [{"inner": "a".repeat(201), "n": 1}, "short"],
            "keep": {"deep": "b".repeat(200)},
        });
        assert!(cut_strings(&mut value, 200));
        let inner = value["outer"][0]["inner"].as_str().unwrap();
        assert_eq!(inner.chars().count(), 200);
        assert!(inner.ends_with('…'));
        assert_eq!(value["outer"][1], "short");
        assert_eq!(value["keep"]["deep"], "b".repeat(200));
        assert_eq!(value["outer"][0]["n"], 1);
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["outer", "keep"]);
        assert!(!cut_strings(
            &mut json!({"a": {"b": ["short", 1, null]}}),
            200
        ));
    }

    #[test]
    fn list_body_fits_and_notes() {
        let items: Vec<Value> = (0..1_000).map(|_| json!("a".repeat(100))).collect();
        let body = list_body("rows", items, None, 200);
        assert_eq!(body["truncated"], json!(true));
        assert!(char_len(&render(&body)) <= MAX_CHARS);
        let notes = body["notes"].as_array().unwrap();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].as_str().unwrap().contains("ask for fewer rows"));
        assert!(body["rows"].as_array().unwrap().len() < 1_000);
    }

    #[test]
    fn list_body_keeps_a_cursor_whole() {
        let cursor = "c".repeat(300);
        let body = list_body("rows", vec![json!(1)], Some(cursor.clone()), 200);
        assert_eq!(body["truncated"], json!(true));
        assert_eq!(body["next_cursor"], json!(cursor));
        assert_eq!(body["notes"], json!([]));
        let body = list_body("rows", vec![json!(1)], None, 200);
        assert_eq!(body["truncated"], json!(false));
        assert_eq!(body["next_cursor"], Value::Null);
    }

    #[test]
    fn list_body_cuts_item_strings_with_a_note() {
        let body = list_body("rows", vec![json!("x".repeat(300))], None, 200);
        // `truncated` is set only by a cursor or by dropped items (the plan's rule).
        assert_eq!(body["truncated"], json!(false));
        assert_eq!(
            body["notes"][0],
            json!("string values longer than 200 characters were cut to 200 characters")
        );
        assert_eq!(body["rows"][0].as_str().unwrap().chars().count(), 200);
    }

    #[test]
    fn object_body_halves_until_it_fits() {
        let mut map = Map::new();
        for i in 0..30 {
            map.insert(format!("s{i:02}"), json!("x".repeat(4_000)));
        }
        let body = object_body(Value::Object(map), 4_000, None);
        assert!(char_len(&render(&body)) <= MAX_CHARS);
        assert_eq!(body["truncated"], json!(true));
        assert_eq!(body["s00"].as_str().unwrap().chars().count(), 500);
        assert!(!body["notes"].as_array().unwrap().is_empty());
    }

    #[test]
    fn object_body_fits_its_list() {
        let turns: Vec<Value> = (0..500).map(|_| json!({"text": "t".repeat(200)})).collect();
        let body = object_body(json!({"turns": turns}), 200, Some("turns"));
        assert!(char_len(&render(&body)) <= MAX_CHARS);
        assert_eq!(body["truncated"], json!(true));
        let notes = body["notes"].as_array().unwrap();
        assert!(
            notes
                .iter()
                .any(|note| note.as_str().unwrap().contains("the list was cut to fit"))
        );
        assert!(body["turns"].as_array().unwrap().len() < 500);
    }

    #[test]
    fn object_body_keeps_its_own_truncated_and_notes() {
        let body = object_body(
            json!({"truncated": true, "notes": ["earlier"], "name": "x"}),
            200,
            None,
        );
        assert_eq!(body["truncated"], json!(true));
        assert_eq!(body["notes"], json!(["earlier"]));
        let body = object_body(json!({"name": "x"}), 200, None);
        assert_eq!(body["truncated"], json!(false));
        assert_eq!(body["notes"], json!([]));
    }
}
