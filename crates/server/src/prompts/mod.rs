//! Prompt validation: names, labels, tags, templates and configs (D-V-2 to D-V-8).
//! No database access.

use std::collections::BTreeSet;
use std::fmt;

use cetra_eval::plain_template::{self, PlainTemplateError, RESERVED};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::api::common::{FieldError, TextPosition};
use crate::meta::prompts::PromptType;

pub(crate) const MAX_NAME_CHARS: usize = 200;
pub(crate) const MAX_DESCRIPTION_CHARS: usize = 2_000;
pub(crate) const MAX_COMMIT_CHARS: usize = 500;
pub(crate) const MAX_TAGS: usize = 20;
pub(crate) const MAX_TAG_CHARS: usize = 64;
pub(crate) const MAX_LABELS_PER_REQUEST: usize = 50;
pub(crate) const MAX_PROTECTED: usize = 20;
pub(crate) const MAX_TEMPLATE_BYTES: usize = 256 * 1024;
pub(crate) const MAX_CHAT_ENTRIES: usize = 200;
pub(crate) const MAX_NAMES: usize = 100;
pub(crate) const MAX_CONFIG_BYTES: usize = 64 * 1024;
pub(crate) const MAX_TOOLS: usize = 128;
/// The label the fetch resolves by default (D-V-9).
pub(crate) const DEFAULT_LABEL: &str = "production";
/// The computed label (D-V-8).
pub(crate) const LATEST: &str = "latest";

/// A chat role (D-V-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatRole {
    System,
    User,
    Assistant,
}

impl ChatRole {
    /// The stored and API spelling.
    const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// One chat entry: a message, or a placeholder that the caller fills with messages (D-V-5).
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ChatEntry {
    /// A message with a role and a template text.
    Message { role: ChatRole, content: String },
    /// A named list of messages supplied at render time.
    Placeholder { name: String },
}

/// Prints the shape only: a template text is payload.
impl fmt::Debug for ChatEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message { role, content } => write!(
                formatter,
                "Message({role:?}, {} chars)",
                content.chars().count()
            ),
            Self::Placeholder { .. } => formatter.write_str("Placeholder"),
        }
    }
}

impl ChatEntry {
    fn to_json(&self) -> Value {
        let mut object = Map::new();
        match self {
            Self::Message { role, content } => {
                object.insert("role".to_owned(), Value::from(role.as_str()));
                object.insert("content".to_owned(), Value::from(content.as_str()));
            }
            Self::Placeholder { name } => {
                object.insert("type".to_owned(), Value::from("placeholder"));
                object.insert("name".to_owned(), Value::from(name.as_str()));
            }
        }
        Value::Object(object)
    }
}

/// A parsed template.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum PromptBody {
    /// A text prompt: one template.
    Text(String),
    /// A chat prompt: 1 to 200 entries.
    Chat(Vec<ChatEntry>),
}

/// Prints the sizes only: a template text is payload.
impl fmt::Debug for PromptBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => write!(formatter, "Text({} chars)", text.chars().count()),
            Self::Chat(entries) => write!(formatter, "Chat({} entries)", entries.len()),
        }
    }
}

impl PromptBody {
    /// The template texts: the text, or the content of each message.
    fn sources(&self) -> Vec<&str> {
        match self {
            Self::Text(text) => vec![text.as_str()],
            Self::Chat(entries) => entries
                .iter()
                .filter_map(|entry| match entry {
                    ChatEntry::Message { content, .. } => Some(content.as_str()),
                    ChatEntry::Placeholder { .. } => None,
                })
                .collect(),
        }
    }

    /// Variable names in order of first appearance across the body.
    pub(crate) fn variables(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        let mut names = Vec::new();
        for source in self.sources() {
            // `parse_body` ran `plain_template::variables` on every source, so this cannot fail.
            for name in plain_template::variables(source).unwrap_or_default() {
                if seen.insert(name.clone()) {
                    names.push(name);
                }
            }
        }
        names
    }

    /// Placeholder names in order of first appearance (empty for a text prompt).
    pub(crate) fn placeholders(&self) -> Vec<String> {
        let Self::Chat(entries) = self else {
            return Vec::new();
        };
        let mut seen = BTreeSet::new();
        let mut names = Vec::new();
        for entry in entries {
            if let ChatEntry::Placeholder { name } = entry
                && seen.insert(name.clone())
            {
                names.push(name.clone());
            }
        }
        names
    }

    /// The JSON stored in `prompt_versions.template` and returned by the API.
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::Text(text) => Value::from(text.as_str()),
            Self::Chat(entries) => Value::Array(entries.iter().map(ChatEntry::to_json).collect()),
        }
    }
}

fn field_error(field: &str, message: &str) -> FieldError {
    FieldError {
        field: field.to_owned(),
        message: message.to_owned(),
        position: None,
    }
}

/// `[A-Za-z_][A-Za-z0-9_]*`, ASCII only: the placeholder name rule.
fn is_placeholder_name(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {
            chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        }
        _ => false,
    }
}

/// D-V-2: 1–200 characters, `[A-Za-z0-9][A-Za-z0-9_.-]*`. Not trimmed (a space is refused).
/// Field `name`.
pub(crate) fn check_name(name: &str) -> Result<(), FieldError> {
    let count = name.chars().count();
    let mut chars = name.chars();
    let starts_well = matches!(chars.next(), Some(first) if first.is_ascii_alphanumeric());
    if !(1..=MAX_NAME_CHARS).contains(&count)
        || !starts_well
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
    {
        return Err(field_error(
            "name",
            "must be 1 to 200 characters: letters, digits, '_', '.', '-', starting with a letter or digit",
        ));
    }
    Ok(())
}

/// D-V-8: `[a-z0-9][a-z0-9_.-]{0,63}`, not only digits, not `latest`. Field as given.
pub(crate) fn check_label(field: &str, label: &str) -> Result<(), FieldError> {
    if label == LATEST {
        return Err(field_error(field, "latest is computed and cannot be set"));
    }
    let count = label.chars().count();
    let starts_well = label
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit());
    if !(1..=64).contains(&count)
        || !starts_well
        || !label.chars().skip(1).all(|ch| {
            ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '_' | '.' | '-')
        })
    {
        return Err(field_error(
            field,
            "must be 1 to 64 characters: lowercase letters, digits, '_', '.', '-'",
        ));
    }
    if label.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(field_error(field, "must not be only digits"));
    }
    Ok(())
}

/// Every label of a request: each one checked (`<field>[i]`), at most `max` of them, no
/// duplicates (`<field>[i]`: "must not repeat a label"). Returned in request order. The count
/// error is on `<field>`.
pub(crate) fn check_labels(
    field: &str,
    labels: &[String],
    max: usize,
) -> Result<Vec<String>, Vec<FieldError>> {
    let mut errors = Vec::new();
    if labels.len() > max {
        errors.push(field_error(
            field,
            &format!("must hold at most {max} labels"),
        ));
    }
    let mut seen = BTreeSet::new();
    for (index, label) in labels.iter().enumerate() {
        let item_field = format!("{field}[{index}]");
        if let Err(error) = check_label(&item_field, label) {
            errors.push(error);
        }
        if !seen.insert(label.as_str()) {
            errors.push(field_error(&item_field, "must not repeat a label"));
        }
    }
    if errors.is_empty() {
        Ok(labels.to_vec())
    } else {
        Err(errors)
    }
}

/// At most 2 000 characters. An empty string is `None`. Field `description`.
pub(crate) fn check_description(text: &str) -> Result<Option<String>, FieldError> {
    if text.is_empty() {
        return Ok(None);
    }
    if text.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(field_error(
            "description",
            "must be at most 2000 characters",
        ));
    }
    Ok(Some(text.to_owned()))
}

/// At most 500 characters. An empty string is `None`. Field `commit_message`.
pub(crate) fn check_commit_message(text: &str) -> Result<Option<String>, FieldError> {
    if text.is_empty() {
        return Ok(None);
    }
    if text.chars().count() > MAX_COMMIT_CHARS {
        return Err(field_error(
            "commit_message",
            "must be at most 500 characters",
        ));
    }
    Ok(Some(text.to_owned()))
}

/// Each tag trimmed and 1–64 characters (`tags[i]`), at most 20 tags in the request (`tags`).
/// Duplicates are dropped, the first kept, and the order is kept.
pub(crate) fn check_tags(tags: &[String]) -> Result<Vec<String>, Vec<FieldError>> {
    let mut errors = Vec::new();
    if tags.len() > MAX_TAGS {
        errors.push(field_error("tags", "must hold at most 20 tags"));
    }
    let mut kept: Vec<String> = Vec::new();
    for (index, tag) in tags.iter().enumerate() {
        let trimmed = tag.trim();
        let count = trimmed.chars().count();
        if !(1..=MAX_TAG_CHARS).contains(&count) {
            errors.push(field_error(
                &format!("tags[{index}]"),
                "must be 1 to 64 characters",
            ));
            continue;
        }
        if !kept.iter().any(|existing| existing == trimmed) {
            kept.push(trimmed.to_owned());
        }
    }
    if errors.is_empty() {
        Ok(kept)
    } else {
        Err(errors)
    }
}

/// A template error as a field error on `field`, with the position of the tag.
fn template_error(field: &str, error: &PlainTemplateError) -> FieldError {
    match error {
        PlainTemplateError::Invalid { at, reason } => FieldError {
            field: field.to_owned(),
            message: reason.message().to_owned(),
            position: Some(TextPosition {
                line: at.line,
                column: at.column,
            }),
        },
        PlainTemplateError::MissingVariable { .. } => field_error(field, "is not a valid template"),
    }
}

/// Checks one template text (D-V-3) and returns it.
fn check_source(field: &str, source: &str) -> Result<String, FieldError> {
    match plain_template::variables(source) {
        Ok(_) => Ok(source.to_owned()),
        Err(error) => Err(template_error(field, &error)),
    }
}

/// Checks one chat entry `template[index]` (D-V-5).
fn parse_entry(index: usize, item: &Value) -> Result<ChatEntry, Vec<FieldError>> {
    let base = format!("template[{index}]");
    let Some(object) = item.as_object() else {
        return Err(vec![field_error(&base, "must be an object")]);
    };
    if object.contains_key("type") {
        let mut errors = Vec::new();
        if object.get("type").and_then(Value::as_str) != Some("placeholder") {
            errors.push(field_error(&format!("{base}.type"), "must be placeholder"));
        }
        if object.len() != 2 || !object.contains_key("name") {
            errors.push(field_error(&base, "a placeholder has only type and name"));
        }
        let name = object.get("name").and_then(Value::as_str);
        match name {
            Some(name) if is_placeholder_name(name) && !RESERVED.contains(&name) => {
                if errors.is_empty() {
                    return Ok(ChatEntry::Placeholder {
                        name: name.to_owned(),
                    });
                }
            }
            _ => errors.push(field_error(
                &format!("{base}.name"),
                "must be a name: letters, digits and _, not starting with a digit",
            )),
        }
        return Err(errors);
    }
    if object.len() != 2 || !object.contains_key("role") || !object.contains_key("content") {
        return Err(vec![field_error(
            &base,
            "a message has only role and content",
        )]);
    }
    let role = match object.get("role").and_then(Value::as_str) {
        Some("system") => Some(ChatRole::System),
        Some("user") => Some(ChatRole::User),
        Some("assistant") => Some(ChatRole::Assistant),
        _ => None,
    };
    let content = object.get("content").and_then(Value::as_str);
    let mut errors = Vec::new();
    if role.is_none() {
        errors.push(field_error(
            &format!("{base}.role"),
            "must be system, user or assistant",
        ));
    }
    match content {
        Some(content) => match check_source(&format!("{base}.content"), content) {
            Ok(content) => {
                if let Some(role) = role
                    && errors.is_empty()
                {
                    return Ok(ChatEntry::Message { role, content });
                }
            }
            Err(error) => errors.push(error),
        },
        None => errors.push(field_error(&format!("{base}.content"), "must be a string")),
    }
    Err(errors)
}

/// Parses and checks a template of `prompt_type` (D-V-3, D-V-5). Every error is returned.
pub(crate) fn parse_body(
    prompt_type: PromptType,
    template: &Value,
) -> Result<PromptBody, Vec<FieldError>> {
    let Ok(serialized) = serde_json::to_string(template) else {
        return Err(vec![field_error("template", "must be valid JSON")]);
    };
    if serialized.len() > MAX_TEMPLATE_BYTES {
        return Err(vec![field_error(
            "template",
            "must be at most 262144 bytes",
        )]);
    }
    let body = match prompt_type {
        PromptType::Text => {
            let Some(source) = template.as_str() else {
                return Err(vec![field_error(
                    "template",
                    "must be a string for a text prompt",
                )]);
            };
            PromptBody::Text(check_source("template", source).map_err(|error| vec![error])?)
        }
        PromptType::Chat => {
            let Some(items) = template.as_array() else {
                return Err(vec![field_error(
                    "template",
                    "must be an array of messages for a chat prompt",
                )]);
            };
            if !(1..=MAX_CHAT_ENTRIES).contains(&items.len()) {
                return Err(vec![field_error("template", "must hold 1 to 200 messages")]);
            }
            let mut errors = Vec::new();
            let mut entries = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                match parse_entry(index, item) {
                    Ok(entry) => entries.push(entry),
                    Err(mut found) => errors.append(&mut found),
                }
            }
            if !errors.is_empty() {
                return Err(errors);
            }
            PromptBody::Chat(entries)
        }
    };
    let variables = body.variables();
    let placeholders = body.placeholders();
    if variables.len() + placeholders.len() > MAX_NAMES {
        return Err(vec![field_error(
            "template",
            "must use at most 100 variables and placeholders",
        )]);
    }
    if variables.iter().any(|name| placeholders.contains(name)) {
        return Err(vec![field_error(
            "template",
            "a name cannot be both a variable and a placeholder",
        )]);
    }
    Ok(body)
}

/// A name of a tool or a response format: 1–64 ASCII letters, digits, `_` or `-`.
fn is_identifier(text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

fn in_range_f64(value: &Value, low: f64, high: f64) -> bool {
    value
        .as_f64()
        .is_some_and(|number| (low..=high).contains(&number))
}

fn in_range_i64(value: &Value, low: i64, high: i64) -> bool {
    value
        .as_i64()
        .is_some_and(|number| (low..=high).contains(&number))
}

/// Checks `config.params` (D-V-6) and pushes one error per bad key.
fn check_params(value: &Value, errors: &mut Vec<FieldError>) {
    let Some(params) = value.as_object() else {
        errors.push(field_error("config.params", "must be a JSON object"));
        return;
    };
    for (key, param) in params {
        let field = format!("config.params.{key}");
        let (ok, rule) = match key.as_str() {
            "temperature" => (in_range_f64(param, 0.0, 2.0), "a number from 0 to 2"),
            "top_p" => (in_range_f64(param, 0.0, 1.0), "a number from 0 to 1"),
            "top_k" => (in_range_i64(param, 1, 1000), "an integer from 1 to 1000"),
            "max_tokens" => (
                in_range_i64(param, 1, 1_000_000),
                "an integer from 1 to 1000000",
            ),
            "presence_penalty" | "frequency_penalty" => {
                (in_range_f64(param, -2.0, 2.0), "a number from -2 to 2")
            }
            "seed" => (param.is_i64(), "an integer"),
            "stop" => (
                match param {
                    Value::String(_) => true,
                    Value::Array(items) => {
                        (1..=4).contains(&items.len()) && items.iter().all(Value::is_string)
                    }
                    _ => false,
                },
                "a string, or an array of 1 to 4 strings",
            ),
            "reasoning_effort" => (
                matches!(param.as_str(), Some("minimal" | "low" | "medium" | "high")),
                "one of minimal, low, medium, high",
            ),
            _ => {
                errors.push(field_error(&field, "is not a known parameter"));
                continue;
            }
        };
        if !ok {
            errors.push(field_error(&field, &format!("must be {rule}")));
        }
    }
}

/// Checks `config.tools` (D-V-6) and returns the tool names, for `tool_choice`.
fn check_tools<'a>(value: &'a Value, errors: &mut Vec<FieldError>) -> Vec<&'a str> {
    let Some(tools) = value.as_array() else {
        errors.push(field_error("config.tools", "must be an array"));
        return Vec::new();
    };
    if tools.len() > MAX_TOOLS {
        errors.push(field_error("config.tools", "must hold at most 128 tools"));
    }
    let mut names: Vec<&str> = Vec::new();
    for (index, tool) in tools.iter().enumerate() {
        let field = format!("config.tools[{index}]");
        let Some(object) = tool.as_object() else {
            errors.push(field_error(&field, "must be an object"));
            continue;
        };
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "name" | "description" | "parameters"))
        {
            errors.push(field_error(
                &field,
                "has only name, description and parameters",
            ));
        }
        let name_field = format!("{field}.name");
        match object.get("name").and_then(Value::as_str) {
            Some(name) if is_identifier(name) => {
                if names.contains(&name) {
                    errors.push(field_error(&name_field, "must repeat no tool name"));
                } else {
                    names.push(name);
                }
            }
            _ => errors.push(field_error(
                &name_field,
                "must be 1 to 64 letters, digits, '_' or '-'",
            )),
        }
        if let Some(description) = object.get("description")
            && !description
                .as_str()
                .is_some_and(|text| text.chars().count() <= 4096)
        {
            errors.push(field_error(
                &format!("{field}.description"),
                "must be a string of at most 4096 characters",
            ));
        }
        if !object.get("parameters").is_some_and(Value::is_object) {
            errors.push(field_error(
                &format!("{field}.parameters"),
                "must be a JSON object",
            ));
        }
    }
    names
}

/// Checks `config.response_format` (D-V-6).
fn check_response_format(value: &Value, errors: &mut Vec<FieldError>) {
    let Some(format) = value.as_object() else {
        errors.push(field_error(
            "config.response_format",
            "must be a JSON object",
        ));
        return;
    };
    if format
        .keys()
        .any(|key| !matches!(key.as_str(), "name" | "schema" | "strict"))
    {
        errors.push(field_error(
            "config.response_format",
            "has only name, schema and strict",
        ));
    }
    if !format
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(is_identifier)
    {
        errors.push(field_error(
            "config.response_format.name",
            "must be 1 to 64 letters, digits, '_' or '-'",
        ));
    }
    if !format.get("schema").is_some_and(Value::is_object) {
        errors.push(field_error(
            "config.response_format.schema",
            "must be a JSON object",
        ));
    }
    if let Some(strict) = format.get("strict")
        && !strict.is_boolean()
    {
        errors.push(field_error(
            "config.response_format.strict",
            "must be true or false",
        ));
    }
}

/// Checks a config object (D-V-6) and returns the text to store: the config as given. Every
/// error is returned. Keys the config does not name are kept as they are.
pub(crate) fn check_config(config: &Value) -> Result<String, Vec<FieldError>> {
    let Some(object) = config.as_object() else {
        return Err(vec![field_error("config", "must be a JSON object")]);
    };
    let Ok(text) = serde_json::to_string(config) else {
        return Err(vec![field_error("config", "must be valid JSON")]);
    };
    let mut errors = Vec::new();
    if text.len() > MAX_CONFIG_BYTES {
        errors.push(field_error("config", "must be at most 65536 bytes"));
    }
    if let Some(model) = object.get("model")
        && !model
            .as_str()
            .is_some_and(|text| (1..=256).contains(&text.chars().count()))
    {
        errors.push(field_error(
            "config.model",
            "must be a string of 1 to 256 characters",
        ));
    }
    if let Some(connection) = object.get("connection_id")
        && !connection
            .as_str()
            .is_some_and(|text| Uuid::parse_str(text).is_ok())
    {
        errors.push(field_error("config.connection_id", "must be a UUID"));
    }
    if let Some(params) = object.get("params") {
        check_params(params, &mut errors);
    }
    let tool_names = match object.get("tools") {
        Some(tools) => check_tools(tools, &mut errors),
        None => Vec::new(),
    };
    if let Some(choice) = object.get("tool_choice")
        && !choice.as_str().is_some_and(|name| {
            matches!(name, "auto" | "none" | "required") || tool_names.contains(&name)
        })
    {
        errors.push(field_error(
            "config.tool_choice",
            "must be auto, none, required or the name of a tool",
        ));
    }
    if let Some(format) = object.get("response_format") {
        check_response_format(format, &mut errors);
    }
    if errors.is_empty() {
        Ok(text)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The one error of a refused value. Panics on zero or several errors.
    fn only_error<T: fmt::Debug>(result: Result<T, Vec<FieldError>>) -> FieldError {
        match result {
            Err(errors) => match errors.as_slice() {
                [error] => error.clone(),
                other => panic!("expected one error, got {other:?}"),
            },
            Ok(value) => panic!("expected an error, got {value:?}"),
        }
    }

    fn at(field: &str, message: &str) -> (String, String) {
        (field.to_owned(), message.to_owned())
    }

    #[test]
    fn names() {
        let long = "a".repeat(200);
        let too_long = "a".repeat(201);
        for accepted in ["a", "support-reply", "v1.2_x", "A", long.as_str()] {
            assert!(check_name(accepted).is_ok(), "{accepted}");
        }
        for refused in ["", " a", "-a", ".a", "a/b", "a b", "é", too_long.as_str()] {
            assert!(check_name(refused).is_err(), "{refused}");
        }
        let error = check_name("a/b").expect_err("refused");
        assert_eq!(error.field, "name");
        assert_eq!(
            error.message,
            "must be 1 to 200 characters: letters, digits, '_', '.', '-', starting with a letter or digit"
        );
    }

    #[test]
    fn labels() {
        let longest = "a".repeat(64);
        let too_long = "a".repeat(65);
        for accepted in ["production", "v1", "a.b-c_d", longest.as_str()] {
            assert!(check_label("label", accepted).is_ok(), "{accepted}");
        }
        for refused in ["Production", "latest", "123", "-x", "", too_long.as_str()] {
            assert!(check_label("label", refused).is_err(), "{refused}");
        }
        assert_eq!(
            check_label("labels[0]", "latest")
                .expect_err("refused")
                .message,
            "latest is computed and cannot be set"
        );
        assert_eq!(
            check_label("labels[0]", "123")
                .expect_err("refused")
                .message,
            "must not be only digits"
        );
        assert_eq!(
            check_label("labels[0]", "Production")
                .expect_err("refused")
                .message,
            "must be 1 to 64 characters: lowercase letters, digits, '_', '.', '-'"
        );
    }

    #[test]
    fn check_labels_counts_and_duplicates() {
        let many: Vec<String> = (0..51).map(|index| format!("l{index}")).collect();
        let error = only_error(check_labels("labels", &many, 50));
        assert_eq!(error.field, "labels");
        assert_eq!(error.message, "must hold at most 50 labels");

        let duplicated = vec!["a".to_owned(), "a".to_owned()];
        let error = only_error(check_labels("labels", &duplicated, 50));
        assert_eq!(error.field, "labels[1]");
        assert_eq!(error.message, "must not repeat a label");

        let ok = vec!["staging".to_owned(), "production".to_owned()];
        assert_eq!(check_labels("labels", &ok, 50), Ok(ok.clone()));
    }

    #[test]
    fn description_and_commit_message_limits() {
        assert_eq!(check_description(""), Ok(None));
        assert_eq!(check_description("text"), Ok(Some("text".to_owned())));
        let long = "a".repeat(MAX_DESCRIPTION_CHARS + 1);
        let error = check_description(&long).expect_err("refused");
        assert_eq!(error.field, "description");
        assert_eq!(error.message, "must be at most 2000 characters");
        assert_eq!(check_commit_message(""), Ok(None));
        let long = "a".repeat(MAX_COMMIT_CHARS + 1);
        let error = check_commit_message(&long).expect_err("refused");
        assert_eq!(error.field, "commit_message");
        assert_eq!(error.message, "must be at most 500 characters");
    }

    #[test]
    fn tags_trim_dedupe_and_cap() {
        let tags = vec![
            " support ".to_owned(),
            "billing".to_owned(),
            "support".to_owned(),
        ];
        assert_eq!(
            check_tags(&tags),
            Ok(vec!["support".to_owned(), "billing".to_owned()])
        );
        let blank = vec!["   ".to_owned()];
        assert_eq!(only_error(check_tags(&blank)).field, "tags[0]");
        let long = vec!["a".repeat(65)];
        assert_eq!(only_error(check_tags(&long)).field, "tags[0]");
        let too_many: Vec<String> = (0..21).map(|index| format!("t{index}")).collect();
        let error = only_error(check_tags(&too_many));
        assert_eq!(error.field, "tags");
        assert_eq!(error.message, "must hold at most 20 tags");
    }

    #[test]
    fn text_body() {
        let body = parse_body(PromptType::Text, &json!("Hi {{ name }}")).expect("valid");
        assert_eq!(body.variables(), vec!["name".to_owned()]);
        assert!(body.placeholders().is_empty());
        assert_eq!(body.to_json(), json!("Hi {{ name }}"));

        let error = only_error(parse_body(PromptType::Text, &json!("{% x")));
        assert_eq!(error.field, "template");
        assert_eq!(error.message, plain_template::Reason::Block.message());
        assert_eq!(error.position, Some(TextPosition { line: 1, column: 1 }));

        let error = only_error(parse_body(PromptType::Text, &json!(42)));
        assert_eq!(error.message, "must be a string for a text prompt");
    }

    fn support_reply_v1() -> Value {
        json!([
            {"role": "system", "content": "You are a support agent for {{ product }}."},
            {"type": "placeholder", "name": "history"},
            {"role": "user", "content": "{{ question }}"}
        ])
    }

    #[test]
    fn chat_body() {
        let template = support_reply_v1();
        let body = parse_body(PromptType::Chat, &template).expect("valid");
        assert_eq!(
            body.variables(),
            vec!["product".to_owned(), "question".to_owned()]
        );
        assert_eq!(body.placeholders(), vec!["history".to_owned()]);
        assert_eq!(body.to_json(), template);
    }

    #[test]
    fn chat_entry_errors() {
        let cases = [
            (json!([]), "template", "must hold 1 to 200 messages"),
            (
                json!([{"role": "tool", "content": "x"}]),
                "template[0].role",
                "must be system, user or assistant",
            ),
            (
                json!([{"role": "user"}]),
                "template[0]",
                "a message has only role and content",
            ),
            (
                json!([{"role": "user", "content": "x", "name": "y"}]),
                "template[0]",
                "a message has only role and content",
            ),
            (
                json!([{"type": "other", "name": "h"}]),
                "template[0].type",
                "must be placeholder",
            ),
            (
                json!([{"type": "placeholder", "name": "1h"}]),
                "template[0].name",
                "must be a name: letters, digits and _, not starting with a digit",
            ),
            (
                json!([{"type": "placeholder", "name": "if"}]),
                "template[0].name",
                "must be a name: letters, digits and _, not starting with a digit",
            ),
            (
                json!([{"role": "user", "content": "{{ h }}"}, {"type": "placeholder", "name": "h"}]),
                "template",
                "a name cannot be both a variable and a placeholder",
            ),
        ];
        for (template, field, message) in cases {
            let error = only_error(parse_body(PromptType::Chat, &template));
            assert_eq!(
                at(&error.field, &error.message),
                at(field, message),
                "{template}"
            );
        }

        let error = only_error(parse_body(
            PromptType::Chat,
            &json!([{"role": "user", "content": "{{ a.b }}"}]),
        ));
        assert_eq!(error.field, "template[0].content");
        assert_eq!(error.position, Some(TextPosition { line: 1, column: 1 }));
    }

    #[test]
    fn body_limits() {
        let big = Value::String("a".repeat(MAX_TEMPLATE_BYTES));
        let error = only_error(parse_body(PromptType::Text, &big));
        assert_eq!(error.field, "template");
        assert_eq!(error.message, "must be at most 262144 bytes");

        let many_entries = Value::Array(vec![json!({"role": "user", "content": "x"}); 201]);
        assert_eq!(
            only_error(parse_body(PromptType::Chat, &many_entries)).message,
            "must hold 1 to 200 messages"
        );

        let names: String = (0..101)
            .map(|index| format!("{{{{ v{index} }}}}"))
            .collect();
        let error = only_error(parse_body(PromptType::Text, &Value::String(names)));
        assert_eq!(
            error.message,
            "must use at most 100 variables and placeholders"
        );
    }

    #[test]
    fn config_rules() {
        let good = json!({
            "model": "gpt-4o-mini",
            "params": {"temperature": 0.2},
        });
        let text = check_config(&good).expect("valid");
        assert_eq!(text, serde_json::to_string(&good).expect("json"));

        let examples = [
            json!({"params": {"top_p": 1, "top_k": 40, "max_tokens": 512, "seed": -3}}),
            json!({"params": {"stop": ["\n\n"], "presence_penalty": -2}}),
            json!({"params": {"stop": "END", "frequency_penalty": 2.0}}),
            json!({"params": {"reasoning_effort": "high"}}),
            json!({
                "connection_id": "01900000-0000-7000-8000-000000000001",
                "tools": [{"name": "get_weather", "description": "Weather", "parameters": {"type": "object"}}],
                "tool_choice": "get_weather",
                "response_format": {"name": "answer", "schema": {"type": "object"}, "strict": true},
            }),
            json!({"tool_choice": "required"}),
        ];
        for example in examples {
            assert!(check_config(&example).is_ok(), "{example}");
        }

        let refused = [
            json!({"params": {"temperature": 3}}),
            json!({"params": {"top_p": -0.1}}),
            json!({"params": {"top_k": 0}}),
            json!({"params": {"max_tokens": 1.5}}),
            json!({"params": {"seed": "x"}}),
            json!({"params": {"stop": ["a", "b", "c", "d", "e"]}}),
            json!({"params": {"reasoning_effort": "max"}}),
            json!({"params": {"foo": 1}}),
            json!({"tools": [{"name": "t", "description": "d"}]}),
            json!({"tools": [
                {"name": "t", "parameters": {}},
                {"name": "t", "parameters": {}}
            ]}),
            json!({"tool_choice": "get_weather"}),
            json!({"response_format": {"name": "answer", "strict": true}}),
            json!({"connection_id": "x"}),
            json!({"model": ""}),
            json!(["not", "an", "object"]),
        ];
        for config in refused {
            assert!(check_config(&config).is_err(), "{config}");
        }

        let error = only_error(check_config(&json!({"params": {"temperature": 3}})));
        assert_eq!(error.field, "config.params.temperature");
        assert_eq!(error.message, "must be a number from 0 to 2");
        let error = only_error(check_config(&json!({"tool_choice": "get_weather"})));
        assert_eq!(error.field, "config.tool_choice");
    }

    #[test]
    fn config_keeps_unknown_top_level_keys() {
        let text = check_config(&json!({"team": "x"})).expect("kept");
        assert!(text.contains("\"team\":\"x\""));
    }

    #[test]
    fn debug_hides_templates() {
        let body = PromptBody::Text("canary-debug".to_owned());
        assert!(!format!("{body:?}").contains("canary-debug"));
        let entry = ChatEntry::Message {
            role: ChatRole::User,
            content: "canary-debug".to_owned(),
        };
        assert!(!format!("{entry:?}").contains("canary-debug"));
        let placeholder = ChatEntry::Placeholder {
            name: "canary-debug".to_owned(),
        };
        assert!(!format!("{placeholder:?}").contains("canary-debug"));
    }
}
