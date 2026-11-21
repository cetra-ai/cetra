//! Plain `{{ name }}` prompt templates (D-V-3): substitution only, no logic.
//!
//! Every template this module accepts is also a valid minijinja template that renders the same
//! output for string values, so a registry prompt can serve as a judge template (D163), with two
//! known differences: minijinja drops one trailing newline of the template (its default
//! `keep_trailing_newline = false`), and it reports the first missing variable in sorted order
//! where this module reports the first in template order. Judges also cap templates at 64 KiB,
//! the registry at 256 KiB. The Python and TypeScript SDKs implement the same rules;
//! `fixtures/prompts/render.json` holds the shared cases.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Names minijinja reads as literals or globals; a variable cannot use them (D-V-3).
pub const RESERVED: &[&str] = &[
    "true",
    "false",
    "none",
    "True",
    "False",
    "None",
    "and",
    "or",
    "not",
    "in",
    "is",
    "if",
    "else",
    "loop",
    "self",
    "range",
    "dict",
    "namespace",
    "debug",
];

/// A 1-based position in characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextAt {
    /// 1-based line.
    pub line: u32,
    /// 1-based column, in characters.
    pub column: u32,
}

/// Why a template was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// `{%` or `{#`.
    Block,
    /// `{{` without a later `}}`.
    Unclosed,
    /// A tag that is not one variable name.
    NotAName,
    /// A name in [`RESERVED`].
    Reserved,
}

impl Reason {
    /// The fixture and API spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Unclosed => "unclosed",
            Self::NotAName => "not_a_name",
            Self::Reserved => "reserved",
        }
    }

    /// A message for an API field error. It never quotes the template.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Block => "{% and {# are not supported; only {{ name }} is",
            Self::Unclosed => "{{ has no closing }}",
            Self::NotAName => {
                "a tag must hold one name: letters, digits and _, not starting with a digit"
            }
            Self::Reserved => "this variable name is reserved",
        }
    }
}

/// A template error. Neither variant holds template text, so it is safe to log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlainTemplateError {
    /// The template does not follow the syntax.
    #[error("invalid template at line {}, column {}: {}", at.line, at.column, reason.as_str())]
    Invalid {
        /// Where the offending tag starts.
        at: TextAt,
        /// Why.
        reason: Reason,
    },
    /// A variable had no value at render time.
    #[error("missing variable {name}")]
    MissingVariable {
        /// The first missing name, in template order.
        name: String,
    },
}

/// One piece of a scanned template.
#[derive(Clone, PartialEq, Eq)]
pub enum Segment<'a> {
    /// Literal text, copied as is.
    Text(&'a str),
    /// A variable name.
    Var(&'a str),
}

/// Prints the length of text, never the text (template text is payload).
impl fmt::Debug for Segment<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => write!(formatter, "Text({} chars)", text.chars().count()),
            Self::Var(name) => write!(formatter, "Var({name})"),
        }
    }
}

/// Splits `source` into text and variables. Adjacent text is one segment; empty text segments
/// are not produced.
///
/// # Errors
///
/// Returns [`PlainTemplateError::Invalid`] at the first `{%`, `{#`, unclosed `{{`, or tag that
/// is not one non-reserved name.
pub fn scan(source: &str) -> Result<Vec<Segment<'_>>, PlainTemplateError> {
    let chars: Vec<(usize, char)> = source.char_indices().collect();
    let mut segments = Vec::new();
    // Byte index where the pending literal text starts.
    let mut text_start = 0usize;
    let mut line: u32 = 1;
    let mut column: u32 = 1;
    let mut index = 0usize;
    while index < chars.len() {
        let (byte, ch) = chars[index];
        if ch == '{' {
            let next = chars.get(index + 1).map(|&(_, next)| next);
            match next {
                Some('%' | '#') => {
                    return Err(PlainTemplateError::Invalid {
                        at: TextAt { line, column },
                        reason: Reason::Block,
                    });
                }
                Some('{') => {
                    let at = TextAt { line, column };
                    let inner_start = byte + 2;
                    // `{` is ASCII, so `inner_start` is a char boundary.
                    let Some(relative_end) = source[inner_start..].find("}}") else {
                        return Err(PlainTemplateError::Invalid {
                            at,
                            reason: Reason::Unclosed,
                        });
                    };
                    let inner_end = inner_start + relative_end;
                    let name = source[inner_start..inner_end].trim_matches([' ', '\t']);
                    if !is_name(name) {
                        return Err(PlainTemplateError::Invalid {
                            at,
                            reason: Reason::NotAName,
                        });
                    }
                    if RESERVED.contains(&name) {
                        return Err(PlainTemplateError::Invalid {
                            at,
                            reason: Reason::Reserved,
                        });
                    }
                    if text_start < byte {
                        segments.push(Segment::Text(&source[text_start..byte]));
                    }
                    segments.push(Segment::Var(name));
                    // A valid tag holds only name characters, spaces and tabs: no newline, so
                    // every char it spans advances the column by one.
                    let after = inner_end + 2;
                    let next_index = chars.partition_point(|&(pos, _)| pos < after);
                    let spanned = u32::try_from(next_index - index).unwrap_or(u32::MAX);
                    column = column.saturating_add(spanned);
                    text_start = after;
                    index = next_index;
                    continue;
                }
                _ => {}
            }
        }
        if ch == '\n' {
            line = line.saturating_add(1);
            column = 1;
        } else {
            column = column.saturating_add(1);
        }
        index += 1;
    }
    if text_start < source.len() {
        segments.push(Segment::Text(&source[text_start..]));
    }
    Ok(segments)
}

/// The variable names, in order of first appearance, without duplicates.
///
/// # Errors
///
/// Returns the first error of [`scan`].
pub fn variables(source: &str) -> Result<Vec<String>, PlainTemplateError> {
    let mut seen = BTreeSet::new();
    let mut names = Vec::new();
    for segment in scan(source)? {
        if let Segment::Var(name) = segment
            && seen.insert(name)
        {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// Renders `source`: each variable is replaced by its value, inserted verbatim (never scanned
/// again). Values not used by the template are ignored.
///
/// # Errors
///
/// Returns the first error of [`scan`]. Before writing anything, every variable is checked: the
/// first one (in template order) missing from `values` is [`PlainTemplateError::MissingVariable`].
pub fn render(
    source: &str,
    values: &BTreeMap<String, String>,
) -> Result<String, PlainTemplateError> {
    Ok(render_up_to(source, values, usize::MAX)?.unwrap_or_default())
}

/// Renders like [`render`], but stops before the output would be longer than `max_bytes`: then
/// it answers `Ok(None)`, having built no more than `max_bytes` bytes. Each piece (a text
/// segment or a value) is checked before it is appended (D-V-37).
///
/// # Errors
///
/// The same errors as [`render`].
pub fn render_up_to(
    source: &str,
    values: &BTreeMap<String, String>,
    max_bytes: usize,
) -> Result<Option<String>, PlainTemplateError> {
    let segments = scan(source)?;
    for segment in &segments {
        if let Segment::Var(name) = segment
            && !values.contains_key(*name)
        {
            return Err(PlainTemplateError::MissingVariable {
                name: (*name).to_owned(),
            });
        }
    }
    let mut out = String::new();
    for segment in segments {
        let piece: &str = match segment {
            Segment::Text(text) => text,
            Segment::Var(name) => values.get(name).map_or("", String::as_str),
        };
        if out.len().saturating_add(piece.len()) > max_bytes {
            return Ok(None);
        }
        out.push_str(piece);
    }
    Ok(Some(out))
}

/// `[A-Za-z_][A-Za-z0-9_]*`, ASCII only.
fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {
            chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;
    use serde_json::Value;

    use super::{PlainTemplateError, Reason, Segment, TextAt, render, scan, variables};
    use crate::PromptTemplate;
    use crate::Variables;
    use crate::error::assert_no_payload;

    const RENDER_CASES: &str = include_str!("../../../fixtures/prompts/render.json");

    fn text_cases() -> Vec<Value> {
        let root: Value = serde_json::from_str(RENDER_CASES).expect("render.json parses");
        root["text"].as_array().expect("text is an array").clone()
    }

    fn values_of(case: &Value) -> BTreeMap<String, String> {
        case["values"]
            .as_object()
            .expect("values is an object")
            .iter()
            .map(|(name, value)| {
                (
                    name.clone(),
                    value.as_str().expect("value is a string").to_owned(),
                )
            })
            .collect()
    }

    fn reason_from(text: &str) -> Reason {
        match text {
            "block" => Reason::Block,
            "unclosed" => Reason::Unclosed,
            "not_a_name" => Reason::NotAName,
            "reserved" => Reason::Reserved,
            other => panic!("unknown reason in render.json: {other}"),
        }
    }

    /// The error a case expects, or `None` for a case that renders.
    fn expected_error(case: &Value) -> Option<PlainTemplateError> {
        let error = case.get("error")?;
        Some(match error["kind"].as_str().expect("kind") {
            "invalid" => PlainTemplateError::Invalid {
                at: TextAt {
                    line: u32::try_from(error["line"].as_u64().expect("line")).expect("u32"),
                    column: u32::try_from(error["column"].as_u64().expect("column")).expect("u32"),
                },
                reason: reason_from(error["reason"].as_str().expect("reason")),
            },
            "missing_variable" => PlainTemplateError::MissingVariable {
                name: error["name"].as_str().expect("name").to_owned(),
            },
            other => panic!("unknown error kind in render.json: {other}"),
        })
    }

    #[test]
    fn shared_text_cases() {
        for case in text_cases() {
            let name = case["name"].as_str().expect("name");
            let template = case["template"].as_str().expect("template");
            let values = values_of(&case);

            let expected_variables: Option<Vec<String>> = match &case["variables"] {
                Value::Null => None,
                list => Some(
                    list.as_array()
                        .expect("variables is an array")
                        .iter()
                        .map(|name| name.as_str().expect("name").to_owned())
                        .collect(),
                ),
            };
            let got_variables = variables(template);
            match expected_variables {
                Some(expected) => assert_eq!(got_variables, Ok(expected), "case {name}"),
                None => assert_eq!(
                    got_variables,
                    Err(expected_error(&case).expect("error")),
                    "case {name}"
                ),
            }

            let expected_render = match expected_error(&case) {
                Some(error) => Err(error),
                None => Ok(case["expected"].as_str().expect("expected").to_owned()),
            };
            assert_eq!(render(template, &values), expected_render, "case {name}");
        }
    }

    #[test]
    fn scan_segments() {
        assert_eq!(
            scan("a{{x}}b{{ y }}").expect("valid"),
            vec![
                Segment::Text("a"),
                Segment::Var("x"),
                Segment::Text("b"),
                Segment::Var("y"),
            ]
        );
        assert_eq!(scan("").expect("valid"), Vec::new());
        assert_eq!(
            scan("{{x}}{{y}}").expect("valid"),
            vec![Segment::Var("x"), Segment::Var("y")]
        );
    }

    #[test]
    fn accepted_templates_render_the_same_in_minijinja() {
        for case in text_cases() {
            let name = case["name"].as_str().expect("name");
            let template = case["template"].as_str().expect("template");
            if case.get("error").is_none() {
                let compiled = PromptTemplate::compile(template)
                    .unwrap_or_else(|err| panic!("case {name}: minijinja refused it: {err}"));
                let values = Variables::from_map(
                    values_of(&case)
                        .into_iter()
                        .map(|(key, value)| (key, Value::String(value)))
                        .collect(),
                );
                let rendered = compiled
                    .render(&values)
                    .unwrap_or_else(|err| panic!("case {name}: minijinja failed: {err}"));
                let mut expected = case["expected"].as_str().expect("expected").to_owned();
                if name == "trailing-newline" {
                    expected = expected
                        .strip_suffix('\n')
                        .expect("the case ends with a newline")
                        .to_owned();
                }
                assert_eq!(rendered, expected, "case {name}");
            }
        }
    }

    #[test]
    fn errors_hold_no_template_text() {
        let err = render("{{ secret_payload.x }}", &BTreeMap::new()).unwrap_err();
        assert!(matches!(err, PlainTemplateError::Invalid { .. }));
        assert_no_payload(&err.to_string(), "secret_payload");

        let err = render("{{secret_payload}}", &BTreeMap::new()).unwrap_err();
        assert_eq!(
            err,
            PlainTemplateError::MissingVariable {
                name: "secret_payload".to_owned(),
            }
        );
    }

    proptest! {
        #![proptest_config(crate::hostile_tests::config())]

        #[test]
        fn render_never_panics(source in any::<String>()) {
            let _ = scan(&source);
            let _ = variables(&source);
            let _ = render(&source, &BTreeMap::new());
        }

        #[test]
        fn one_variable_is_substituted(
            before in "[a-zA-Z0-9 \n]{0,40}",
            after in "[a-zA-Z0-9 \n]{0,40}",
        ) {
            let source = format!("{before}{{{{v0}}}}{after}");
            let values = BTreeMap::from([("v0".to_owned(), "x".to_owned())]);
            prop_assert_eq!(render(&source, &values), Ok(format!("{before}x{after}")));
        }
    }
}
